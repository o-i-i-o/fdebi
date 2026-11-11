//! varisat(CDCL SAT)编码的依赖求解器。
//!
//! 求解域:已安装库(可卸载)∪ 仓库候选 ∪ 本地 deb(强制)。
//! 编码:
//! - 变量 = 选择单元:已安装包"保持安装"或某候选版本"被选中"
//! - 同名二进制包(键)单元两两互斥(AMO)
//! - Depends/Pre-Depends:选中 → 每个依赖组至少一个满足者(直接命中或虚拟提供);
//!   架构限定按 Debian Multi-Arch 规则匹配
//! - Conflicts/Breaks:互斥子句(Replaces 不豁免:dpkg 会卸载被替换包)
//! - Essential/Protected 包强制同键包保持存在(保护性约束)
//!
//! 求解策略:已装包默认全部保持(apt 语义快路径)→ 必要时退回假设式
//! 迭代精化(最小化卸载、新装键优先新版本)。
//!
//! 失败原因统一为"求解失败"类消息(不逐条罗列);or 组解析交给 SAT
//! 而非首分支回溯。

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use varisat::{ExtendFormula, Lit, Solver};

use crate::debfile::DebFile;
use crate::error::FdebmError;
use crate::i18n::{gettext, tf};
use crate::sysstate::{Dep, Pkg, SysState, base_name};
use crate::version::cmp_version;

/// 下载坐标(仓库候选被选中时生成;本地 deb 直接走 dpkg -i)
#[derive(Debug, Clone)]
pub struct DownloadSpec {
    pub name: String,
    pub version: String,
    pub url: String,
    pub dest: PathBuf,
    pub sha256: String,
    pub size: u64,
    pub trusted: bool,
}

/// 求解结果中的变更集(名称列表,GUI 呈现用)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    pub install: Vec<String>,
    pub remove: Vec<String>,
    pub unauthenticated: Vec<String>,
}

/// 完整求解结果
#[derive(Debug, Clone)]
pub struct Resolution {
    pub changes: Changes,
    pub downloads: Vec<DownloadSpec>,
}

/// 依赖检查结论
#[derive(Debug, Clone)]
pub enum Verdict {
    Installable(Resolution),
    Uninstallable(String),
}

/// 选择单元;编号空间:Installed 0..n,随后 Candidate
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Unit {
    /// installed 下标:该包保持安装
    Installed(usize),
    /// all_candidates 下标:该候选被选中
    Candidate(usize),
}

/// SAT 编码器:单元编号 ↔ varisat 变量(var 编号 = 单元编号 + 1)
struct Encoding<'a> {
    sys: &'a SysState,
    /// 本地 deb(独立单元,排在候选之后;不落入 sys 的缓存对象)。
    /// 借用而非持有:索引键可直接借用 sys/local 中的字符串,避免为数万
    /// 单元逐个 clone 包名/键。
    local: Option<&'a crate::sysstate::Candidate>,
    n_installed: usize,
    n_cands: usize,
    /// 虚拟名 → 提供者单元;HashMap 仅做点查(不迭代),比 BTreeMap
    /// 在数万候选规模下插入/查找更快
    providers: HashMap<&'a str, Vec<Unit>>,
    /// 包名 → 全部架构/版本的单元
    by_name: HashMap<&'a str, Vec<Unit>>,
    /// 键 → 单元(AMO 分组)
    groups: HashMap<&'a str, Vec<Unit>>,
}

impl<'a> Encoding<'a> {
    fn new(sys: &'a SysState, local: Option<&'a crate::sysstate::Candidate>) -> Self {
        let n_installed = sys.installed.len();
        let n_cands = sys.all_candidates.len();
        let cap = n_installed + n_cands + usize::from(local.is_some());
        let mut providers: HashMap<&str, Vec<Unit>> = HashMap::with_capacity(cap);
        let mut by_name: HashMap<&str, Vec<Unit>> = HashMap::with_capacity(cap);
        let mut groups: HashMap<&str, Vec<Unit>> = HashMap::with_capacity(cap);
        let mut push = |u: Unit, pkg: &'a Pkg| {
            by_name.entry(pkg.name.as_str()).or_default().push(u);
            for p in &pkg.provides {
                providers.entry(p.0.as_str()).or_default().push(u);
            }
            groups.entry(pkg.key.as_str()).or_default().push(u);
        };
        for (i, pkg) in sys.installed.iter().enumerate() {
            push(Unit::Installed(i), pkg);
        }
        for (c, cand) in sys.all_candidates.iter().enumerate() {
            push(Unit::Candidate(c), &cand.pkg);
        }
        if let Some(local) = local {
            push(Unit::Candidate(n_cands), &local.pkg);
        }
        Self {
            sys,
            local,
            n_installed,
            n_cands,
            providers,
            by_name,
            groups,
        }
    }

    fn unit(&self, idx: usize) -> Unit {
        if idx < self.n_installed {
            Unit::Installed(idx)
        } else {
            Unit::Candidate(idx - self.n_installed)
        }
    }

    /// 单元 → 统一编号(SAT 模型下标空间:Installed 0..n,随后 Candidate)
    fn unit_idx(&self, u: Unit) -> usize {
        match u {
            Unit::Installed(i) => i,
            Unit::Candidate(c) => self.n_installed + c,
        }
    }

    fn plit(&self, idx: usize) -> Lit {
        Lit::from_dimacs((idx + 1) as isize)
    }

    fn nlit(&self, idx: usize) -> Lit {
        Lit::from_dimacs(-((idx + 1) as isize))
    }

    fn meta(&self, u: Unit) -> &Pkg {
        match u {
            Unit::Installed(i) => &self.sys.installed[i],
            Unit::Candidate(c) => {
                if c >= self.n_cands {
                    &self.local.expect("local unit present").pkg
                } else {
                    &self.sys.all_candidates[c].pkg
                }
            }
        }
    }

    /// 依赖满足者:直接命中与虚拟提供的并集(or 组取舍交给 SAT)。
    /// 曾用"直连优先、无直连才看提供":仓库存在同名候选时已装提供者
    /// 被排除出依赖子句,keep-all 模型被迫安装无关候选(如已由
    /// sysvinit-utils 提供的 lsb-base 仍被要求安装仓库候选)。
    /// 版本化约束经虚拟提供命中时要求版本化提供(dpkg ≥1.17.11 语义)。
    /// depender_arch 是发起依赖的包自身架构(arch:all 由匹配函数按 native 处理),
    /// 无限定依赖的跨架构匹配必须据此判断。
    ///
    /// 复用调用方缓冲(依赖子句生成在真实索引规模下调用数十万次,
    /// 逐次分配是 resolve 的主要开销)。
    fn satisfiers_into(&self, d: &Dep, depender_arch: &str, out: &mut Vec<Unit>) {
        out.clear();
        if let Some(units) = self.by_name.get(base_name(&d.name)) {
            out.extend(units.iter().copied().filter(|&u| {
                let pkg = self.meta(u);
                dep_name_matches(&d.name, pkg, depender_arch, self.sys.native_arch.as_str())
                    && ver_matches(&pkg.version, &d.oper, &d.ver)
            }));
        }
        if let Some(units) = self.providers.get(base_name(&d.name)) {
            out.extend(
                units
                    .iter()
                    .copied()
                    .filter(|&u| self.provide_matches(u, d, depender_arch)),
            );
        }
    }

    /// 提供命中:虚拟名 + 架构规则 + 版本约束(无版本约束恒真;
    /// 有版本约束时必须存在匹配的版本化提供)
    fn provide_matches(&self, u: Unit, d: &Dep, depender_arch: &str) -> bool {
        let pkg = self.meta(u);
        if !dep_arch_matches(&d.name, pkg, depender_arch, self.sys.native_arch.as_str()) {
            return false;
        }
        if d.ver.is_empty() || d.oper.is_empty() {
            return true;
        }
        pkg.provides.iter().any(|(n, pv)| {
            n == base_name(&d.name)
                && pv
                    .as_ref()
                    .is_some_and(|pv| ver_matches(pv, &d.oper, &d.ver))
        })
    }

    /// U 的 Conflicts/Breaks 条目 c 是否命中单元 v。
    /// 冲突关系的架构语义与依赖不同:无限定名匹配任意架构,
    /// ":any" 也匹配任意架构(Debian Policy 7.1;已用 dpkg 实测)。
    fn hits(&self, c: &Dep, v: Unit) -> bool {
        let native = self.sys.native_arch.as_str();
        let pkg = self.meta(v);
        if relation_covers(&c.name, pkg, native) {
            c.ver.is_empty() || ver_matches(&pkg.version, &c.oper, &c.ver)
        } else if self
            .providers
            .get(base_name(&c.name))
            .is_some_and(|ps| ps.contains(&v))
        {
            // 提供命中:版本化约束仅适用版本化提供
            // (否则 dpkg 的版本化 Breaks 会误伤无版本提供者,如
            //  dpkg Breaks lsb-base (<< x) 对 sysvinit-utils 的无版本提供)
            relation_arch_matches(&c.name, pkg, native) && self.provide_ver_matches(v, c)
        } else {
            false
        }
    }

    /// 提供命中时的版本约束检查(架构由调用方另行检查)
    fn provide_ver_matches(&self, u: Unit, d: &Dep) -> bool {
        if d.ver.is_empty() || d.oper.is_empty() {
            return true;
        }
        self.meta(u).provides.iter().any(|(n, pv)| {
            n == base_name(&d.name)
                && pv
                    .as_ref()
                    .is_some_and(|pv| ver_matches(pv, &d.oper, &d.ver))
        })
    }

    /// 单个依赖组的子句:¬idx ∨ 各备选的满足者(与全量编码同构)
    fn dep_group_clause(
        &self,
        idx: usize,
        group: &[Dep],
        solver: &mut Solver,
        lits: &mut Vec<Lit>,
        sats: &mut Vec<Unit>,
    ) {
        let depender_arch = self.meta(self.unit(idx)).arch.as_str();
        lits.clear();
        lits.push(self.nlit(idx));
        for d in group {
            sats.clear();
            self.satisfiers_into(d, depender_arch, sats);
            lits.extend(sats.iter().map(|&s| self.plit_of(s)));
        }
        add_clause(solver, lits);
    }

    /// 单元的全部依赖子句(含 Pre-Depends)
    fn add_dep_clauses(
        &self,
        idx: usize,
        solver: &mut Solver,
        lits: &mut Vec<Lit>,
        sats: &mut Vec<Unit>,
    ) {
        let meta = self.meta(self.unit(idx));
        for group in meta.depends.iter().chain(meta.pre_depends.iter()) {
            self.dep_group_clause(idx, group, solver, lits, sats);
        }
    }

    /// 依赖闭包:从种子单元出发,经 Depends/Pre-Depends 的全部满足者
    /// 可达的单元集(satisfiers 含直接命中与虚拟提供,覆盖全部候选版本)
    fn reachable_closure(&self, seeds: impl IntoIterator<Item = usize>) -> HashSet<usize> {
        let mut seen: HashSet<usize> = seeds.into_iter().collect();
        let mut queue: Vec<usize> = seen.iter().copied().collect();
        let mut sats: Vec<Unit> = Vec::new();
        while let Some(idx) = queue.pop() {
            let meta = self.meta(self.unit(idx));
            let depender_arch = meta.arch.as_str();
            for group in meta.depends.iter().chain(meta.pre_depends.iter()) {
                for d in group {
                    self.satisfiers_into(d, depender_arch, &mut sats);
                    for u in sats.drain(..) {
                        let ui = self.unit_idx(u);
                        if seen.insert(ui) {
                            queue.push(ui);
                        }
                    }
                }
            }
        }
        seen
    }
}

/// 添加子句(排序去重,空子句即 UNSAT 由求解器报告);复用调用方缓冲
fn add_clause(solver: &mut Solver, lits: &mut Vec<Lit>) {
    lits.sort();
    lits.dedup();
    solver.add_clause(lits);
}

/// 依赖的架构限定是否命中给定包(Debian Multi-Arch 规范,已用 dpkg 实测):
/// - 无限定:同架构(arch:all 依赖方按 native 处理)/ arch:all /
///   Multi-Arch: foreign;
/// - `:any`:仅 Multi-Arch: allowed(arch:all 也需标记 allowed);
/// - 显式架构:精确匹配;显式本机架构同时接受 arch:all。
///
/// `:native` 不是二进制依赖的限定符(dpkg 1.23 只认真实架构名与 `any`,
/// 实测将其当作字面架构名 "native",永不命中;该写法仅用于构建依赖),
/// 因此这里不特殊处理,按显式架构规则匹配。
fn dep_arch_matches(dep_name: &str, pkg: &Pkg, depender_arch: &str, native_arch: &str) -> bool {
    let depender_arch = if depender_arch.is_empty() || depender_arch == "all" {
        native_arch
    } else {
        depender_arch
    };
    match dep_name.split_once(':').map(|(_, a)| a) {
        Some("any") => pkg.multi_arch == "allowed",
        Some(a) => pkg.arch == a || (a == native_arch && pkg.arch == "all"),
        None => pkg.arch == depender_arch || pkg.arch == "all" || pkg.multi_arch == "foreign",
    }
}

/// 依赖名(含 :arch 限定)是否命中包的 name 与架构
fn dep_name_matches(dep_name: &str, pkg: &Pkg, depender_arch: &str, native_arch: &str) -> bool {
    pkg.name == base_name(dep_name) && dep_arch_matches(dep_name, pkg, depender_arch, native_arch)
}

/// Conflicts/Breaks 的架构限定是否命中(与依赖规则不同):
/// 无限定与 `:any` 均匹配任意架构;显式本机架构接受 arch:all;
/// 其余显式架构精确匹配。`:native` 同样按字面架构名处理(见
/// [`dep_arch_matches`])。已用 dpkg 实测。
fn relation_arch_matches(dep_name: &str, pkg: &Pkg, native_arch: &str) -> bool {
    match dep_name.split_once(':').map(|(_, a)| a) {
        None | Some("any") => true,
        Some(a) => pkg.arch == a || (a == native_arch && pkg.arch == "all"),
    }
}

/// 冲突名是否命中包的 name 与架构
fn relation_covers(dep_name: &str, pkg: &Pkg, native_arch: &str) -> bool {
    pkg.name == base_name(dep_name) && relation_arch_matches(dep_name, pkg, native_arch)
}

/// dpkg 关系操作符版本匹配("<"/">" 为 "<<"(">>") 的旧写法)
fn ver_matches(v: &str, oper: &str, w: &str) -> bool {
    if oper.is_empty() || w.is_empty() {
        return true;
    }
    let c = cmp_version(v, w);
    match oper {
        "<=" => c != std::cmp::Ordering::Greater,
        ">=" => c != std::cmp::Ordering::Less,
        "<<" | "<" => c == std::cmp::Ordering::Less,
        ">>" | ">" => c == std::cmp::Ordering::Greater,
        "=" => c == std::cmp::Ordering::Equal,
        _ => true,
    }
}

/// 主入口:在给定系统状态上求解安装 deb(不修改 sys)。
pub fn resolve(sys: &SysState, deb: &DebFile) -> Result<Verdict, FdebmError> {
    let name = deb.field_trimmed("Package").unwrap_or("").to_string();
    let version = deb.field_trimmed("Version").unwrap_or("").to_string();
    let arch = deb
        .field_trimmed("Architecture")
        .unwrap_or("all")
        .to_string();

    // control 缺少 Package/Version 时不能进入求解:local_candidate 需要
    // 这些字段,旧实现在这里 expect 会 panic(守护进程处理用户文件时是
    // 可达路径)。dpkg 同样拒绝这种包。
    if name.is_empty() || version.is_empty() {
        return Ok(Verdict::Uninstallable(gettext(
            "Invalid package: missing Package or Version field",
        )));
    }

    // 架构检查(dpkg 语义)
    let mut archs = vec![sys.native_arch.clone(), "all".to_string()];
    archs.extend(sys.foreign_arches.iter().cloned());
    if !archs.contains(&arch) {
        return Ok(Verdict::Uninstallable(tf(
            "Wrong architecture '{0}'",
            &[&arch],
        )));
    }

    // 降级拒绝(dpkg 语义;重装同版本放行)
    let key = if arch == "all" || arch == sys.native_arch {
        name.clone()
    } else {
        format!("{}:{}", name, arch)
    };
    if let Some(installed) = sys.installed_version(&key)
        && cmp_version(installed, &version) == std::cmp::Ordering::Greater
    {
        return Ok(Verdict::Uninstallable(gettext(
            "A later version is already installed",
        )));
    }

    let local = local_candidate(deb, &sys.native_arch);
    Ok(encode_and_solve(sys, &local))
}

/// 本地 deb → 候选元数据(键派生与 SysState::load 同规则)
fn local_candidate(deb: &DebFile, native_arch: &str) -> crate::sysstate::Candidate {
    let pkg = crate::sysstate::pkg_from_fields(|n| deb.field_trimmed(n), native_arch)
        .expect("DebFile::open 已校验 Package 字段");
    crate::sysstate::Candidate {
        pkg,
        repo: None,
        repo_filename: String::new(),
        sha256: String::new(),
        size: 0,
        local_path: Some(deb.path().to_path_buf()),
    }
}

/// SAT 模型 → 必要选中集(贪心闭包提取)。
///
/// 求解器返回的是全变量赋值模型:与安装无关的自由变量可能被启发式
/// 置真(真实索引规模下曾把 qemu-hwe/dbus-x11 等无关候选拉进变更集),
/// 且已装包的依赖组即使已有保持的已装满足者,模型也可能冗余置真其他
/// 候选满足者。这里从模型真值中的"保持的已装单元 ∪ 本地 deb"出发,
/// 逐组贪心:组已被必要集满足则不新增;否则按 satisfiers 顺序(直连
/// 在前、贴近 apt 的命名偏好)取首个模型置真的满足者。所得子集仍是
/// 原公式模型:每个必需单元的每个依赖组都有必要集内的满足者;
/// AMO/冲突子句对真值子集封闭(子集不引入新的同为真对)。
fn extract_needed(enc: &Encoding<'_>, model: &HashSet<usize>) -> BTreeSet<usize> {
    // 已装保持集:模型真值中的已装单元全部保留(不参与可达性裁剪)。
    // BTreeSet 保证结果按单元编号确定有序(GUI/下载/安装顺序可复现)。
    let mut needed: BTreeSet<usize> = (0..enc.n_installed).filter(|i| model.contains(i)).collect();
    if enc.local.is_some() {
        needed.insert(enc.n_installed + enc.n_cands);
    }
    let mut queue: Vec<usize> = needed.iter().copied().collect();
    // 复用 satisfier 缓冲:每个依赖组只查一次,避免 satisfiers() 的重复分配。
    let mut sats: Vec<Unit> = Vec::new();
    while let Some(idx) = queue.pop() {
        let meta = enc.meta(enc.unit(idx));
        let depender_arch = meta.arch.as_str();
        for group in meta.depends.iter().chain(meta.pre_depends.iter()) {
            let mut satisfied = false;
            let mut chosen = None;
            for d in group {
                enc.satisfiers_into(d, depender_arch, &mut sats);
                // 组已被必要集满足则不新增(模型可能冗余置真其他满足者)
                if sats.iter().any(|&s| needed.contains(&enc.unit_idx(s))) {
                    satisfied = true;
                    break;
                }
                // 组未被满足:按 satisfiers 顺序取首个模型置真的满足者
                if chosen.is_none() {
                    chosen = sats
                        .iter()
                        .map(|&s| enc.unit_idx(s))
                        .find(|&s_idx| model.contains(&s_idx));
                }
            }
            if satisfied {
                continue;
            }
            if let Some(s_idx) = chosen
                && needed.insert(s_idx)
            {
                queue.push(s_idx);
            }
        }
    }
    needed
}

/// 假设求解 + 模型验证(依赖子句惰性编码的收敛循环)。
///
/// 子句库只含已装单元与本地 deb 的全部依赖子句;仓库候选的依赖子句按需
/// 补加:每轮只处理"模型真值中尚未编码的单元",为其全部依赖组补加子句
/// 后重解(varisat 增量接口,已学子句保留)。子句一旦入库,「单元为真 ⇒
/// 组内必有真满足者」由公式自身保证,已编码单元无需随模型重查 —— 曾按
/// "在模型内已满足则跳过补码"验证:满足者无子句钉住,下一轮被求解器
/// 翻转,组再次失配,收敛循环可跑上千轮并全量重查真值单元(真实索引
/// 规模下 resolve 超 20s,其中 satisfiers 查找占九成)。
/// - 返回 None:当前子句库在假设下 UNSAT —— 子句库 ⊆ 全公式,子句集越小
///   可满足性越宽松,故全公式必 UNSAT,结论可直接采信;
/// - 返回 Some:验证通过时模型满足全公式(假值单元的依赖子句被 ¬u 平凡
///   满足);返回原始模型,必要集由调用方按需提取(逐包保持试探只消费
///   "已装单元是否保留",全量提取留到最后做一次)。
///
/// 验证按单元编号升序扫描,同一输入的补加次序确定,结果可复现。
fn solve_verified(
    solver: &mut Solver,
    enc: &Encoding<'_>,
    forced: &[Lit],
    encoded: &mut HashSet<usize>,
    lits: &mut Vec<Lit>,
    sats: &mut Vec<Unit>,
) -> Option<HashSet<usize>> {
    loop {
        solver.assume(forced);
        if !solver.solve().unwrap_or(false) {
            return None;
        }
        let model: HashSet<usize> = solver
            .model()
            .unwrap_or_default()
            .iter()
            .filter(|l| l.is_positive())
            .map(|l| l.index())
            .collect();
        let mut true_units: Vec<usize> = model.iter().copied().collect();
        true_units.sort_unstable();
        let mut added = false;
        for idx in true_units {
            // 已编码单元的依赖子句在库内,任何模型都满足它,直接跳过
            if !encoded.insert(idx) {
                continue;
            }
            enc.add_dep_clauses(idx, solver, lits, sats);
            added = true;
        }
        if !added {
            return Some(model);
        }
    }
}

fn encode_and_solve(sys: &SysState, local: &crate::sysstate::Candidate) -> Verdict {
    let enc = Encoding::new(sys, Some(local));
    let n_units = enc.n_installed + enc.n_cands + usize::from(enc.local.is_some());
    let local_idx = enc
        .local
        .as_ref()
        .map(|_| enc.n_installed + enc.n_cands)
        .expect("resolve 已注入本地候选");

    let mut solver = Solver::new();
    let mut lits: Vec<Lit> = Vec::new();
    let mut sats: Vec<Unit> = Vec::new();

    // 目标:本地 deb 强制选中
    lits.push(enc.plit(local_idx));
    add_clause(&mut solver, &mut lits);

    // Essential/Protected 包强制保持安装(保护性约束,dpkg 语义:
    // Essential 是 --force-remove-essential、Protected 是 --force-remove-protected
    // 的守门对象;Protected: yes 同样包括 dpkg 兼容的旧写法 Important: yes)。
    // 约束对象是“同 key 的包仍然存在”,而不是“这个已装单元必须为真”:
    // 升级/重装 Essential 包时本地 deb 与已装单元同 key 互斥,若强制旧单元
    // 为真会与本地强制选择 + AMO 直接 UNSAT(曾导致 bash/dpkg 升级误报不可安装)。
    for (i, pkg) in sys.installed.iter().enumerate() {
        if !(pkg.essential || pkg.protected) {
            continue;
        }
        lits.clear();
        match enc.groups.get(pkg.key.as_str()) {
            Some(units) => lits.extend(units.iter().map(|&u| enc.plit_of(u))),
            None => lits.push(enc.plit(i)),
        }
        add_clause(&mut solver, &mut lits);
    }

    // 同键互斥(AMO)。groups 是 HashMap:只对多单元组按 key 排序后编码,
    // 既跳过绝大多数单候选键,也保持子句加入顺序确定(结果可复现)。
    let mut multi_groups: Vec<&str> = enc
        .groups
        .iter()
        .filter(|(_, group)| group.len() > 1)
        .map(|(key, _)| *key)
        .collect();
    multi_groups.sort_unstable();
    for key in multi_groups {
        let group = &enc.groups[key];
        for a in 0..group.len() {
            for b in a + 1..group.len() {
                lits.clear();
                lits.push(enc.nlit_of(group[a]));
                lits.push(enc.nlit_of(group[b]));
                add_clause(&mut solver, &mut lits);
            }
        }
    }

    // 依赖子句(含 Pre-Depends,配置顺序由 dpkg 保证)。
    // 惰性编码(CEGAR):仅已装单元与本地 deb 全量编码 —— keep-all 假设下
    // 已装恒真、本地必选,子句必然起作用;仓库候选(数万单元)的依赖子句
    // 只有该候选为真时才影响可满足性,交由 solve_verified 按模型验证补加
    // (真实索引规模下全量编码曾占 resolve 耗时的七成)
    for idx in 0..enc.n_installed {
        enc.add_dep_clauses(idx, &mut solver, &mut lits, &mut sats);
    }
    enc.add_dep_clauses(local_idx, &mut solver, &mut lits, &mut sats);
    // 已装单元与本地 deb 的依赖子句已全量入库,标记为已编码
    let mut encoded: HashSet<usize> = (0..enc.n_installed).collect();
    encoded.insert(local_idx);

    // 可达性剪枝(先于冲突编码计算):varisat 对无约束变量的默认启发式是
    // 置真,未剪枝时每轮收敛都可能把无关候选拉进模型并级联编码其依赖,
    // 收敛轮数与索引规模同阶(实测 1500+ 轮求解、千万次 satisfiers 查找)。
    // 合法方案中的候选必然落在「已装单元 ∪ 本地 deb」的依赖闭包内,
    // 闭包外候选直接 ¬u 封死:不排除任何可行解,只缩小搜索空间。
    let reachable = enc.reachable_closure((0..enc.n_installed).chain(std::iter::once(local_idx)));
    let mut reachable_flag = vec![false; n_units];
    for &idx in &reachable {
        reachable_flag[idx] = true;
    }

    // Conflicts 与 Breaks(按名字索引取 victim,避免全单元平方级扫描)。
    // 两者对 solver 同构(apt 解析语义:breaks 也是互斥约束),作用于全部单元
    // —— 已装×候选(如已装 dpkg Breaks 旧版 lsb-base,候选 lsb-base 不得
    // 被选中降 dead dpkg)与候选×候选(同一变更集内互不相容)都必须编码,
    // 否则违规要到 dpkg 配置阶段(--auto-deconfigure 会把已装受害方降置
    // deconfigured)才暴露。仅"双方均已安装"的存量违规不回溯修复:
    // dpkg 不会重新校验存量态,历史冲突不应当阻断 keep-all 快路径。
    for (idx, &is_reachable) in reachable_flag.iter().enumerate() {
        // 闭包外候选会被 ¬u 封死,其冲突/破坏子句恒被满足,无需编码。
        // 真实索引里可达单元通常只占候选总量的个位数百分比。
        if idx >= enc.n_installed && !is_reachable {
            continue;
        }
        let u = enc.unit(idx);
        let meta = enc.meta(u);
        if meta.conflicts.is_empty() && meta.breaks.is_empty() {
            continue;
        }
        for deps in meta.conflicts.iter().chain(meta.breaks.iter()) {
            for c in deps {
                // hits() 的名字命中 = 直接同名 ∪ 虚拟提供,victim 集按此索引
                let base = base_name(&c.name);
                let mut victims: Vec<Unit> = enc.by_name.get(base).cloned().unwrap_or_default();
                victims.extend(enc.providers.get(base).cloned().unwrap_or_default());
                for v in victims {
                    if v == u {
                        continue;
                    }
                    // 双方均已安装:存量态dpkg不会重新校验(apt 亦不重排),
                    // 本事务只需不新增违规,历史违规不回溯修复 —— 否则
                    // 真实系统的存量冲突对会破坏 keep-all 快路径
                    if matches!(u, Unit::Installed(_)) && matches!(v, Unit::Installed(_)) {
                        continue;
                    }
                    // 注意:Replaces 不豁免冲突子句。dpkg 对
                    // Conflicts+Replaces 的实际动作是卸载被替换包(必要时
                    // --auto-deconfigure 反配置其反向依赖),不是允许两者共存。
                    // 曾经的豁免让模型保留受害方、漏掉 remove,反向依赖被
                    // 误判满足,最终 dpkg -i 失败。
                    if enc.hits(c, v) {
                        let v_idx = match v {
                            Unit::Installed(i) => i,
                            Unit::Candidate(c) => enc.n_installed + c,
                        };
                        lits.clear();
                        lits.push(enc.nlit(idx));
                        lits.push(enc.nlit(v_idx));
                        add_clause(&mut solver, &mut lits);
                    }
                }
            }
        }
    }

    // 快路径(apt 语义):已装包默认全部保持安装,卸载仅在冲突/换版强制时发生。

    // 闭包外候选封死(不排除任何可行解,只缩小搜索空间)
    for c in 0..enc.n_cands {
        let idx = enc.n_installed + c;
        if !reachable_flag[idx] {
            lits.clear();
            lits.push(enc.nlit(idx));
            add_clause(&mut solver, &mut lits);
        }
    }

    // keep-all 快路径假设。本地 deb 与同键已装单元在 AMO 组内互斥:
    // 重装/升级/降级场景下若把该已装单元一并保留,快路径必然 UNSAT 而
    // 跌入逐包试探(O(已装数) 次求解)。这些单元无论如何都得让位
    // (remove 组装同样视"被同键新候选替换"为非卸载),排除不影响任何可行解。
    let local_key = enc.meta(enc.unit(local_idx)).key.as_str();
    let same_key: HashSet<usize> = enc
        .groups
        .get(local_key)
        .map(|units| {
            units
                .iter()
                .filter_map(|u| match u {
                    Unit::Installed(i) => Some(*i),
                    Unit::Candidate(_) => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let keeps: Vec<Lit> = (0..enc.n_installed)
        .filter(|i| !same_key.contains(i))
        .map(|i| enc.plit(i))
        .collect();
    // keep-all 失败时,从 UNSAT 失败核中逐个放宽假设,而不是退化为
    // “无假设模型 + 逐包尝试保留”。后者在真实系统上要对数百个被启发式
    // 置假的已装包各做一次增量求解(实测单次 resolve 约 627 次),是冲突
    // 场景的主要开销。核放宽每轮至少去掉一个导致冲突的保留项。
    let mut forced: Vec<Lit> = keeps;
    let mut relaxed: Vec<usize> = Vec::new();
    let mut model: HashSet<usize> = loop {
        if let Some(m) = solve_verified(
            &mut solver,
            &enc,
            &forced,
            &mut encoded,
            &mut lits,
            &mut sats,
        ) {
            break m;
        }
        let pick = {
            let core = solver.failed_core().unwrap_or(&[]);
            core.iter()
                .filter(|lit| lit.is_positive())
                .map(|lit| lit.index())
                .find(|&i| i < enc.n_installed && !relaxed.contains(&i))
        };
        let Some(i) = pick else {
            return Verdict::Uninstallable(gettext(
                "Failed to satisfy all dependencies (conflicting requests or missing packages)",
            ));
        };
        relaxed.push(i);
        let lit = enc.plit(i);
        if let Some(pos) = forced.iter().position(|&l| l == lit) {
            forced.swap_remove(pos);
        }
    };
    // 失败核可能比必要集更大:逐个把放宽项加回,仍然得到最少卸载方案。
    // relaxed 通常只有个位数项,额外求解成本可忽略。
    for &i in &relaxed {
        let mut assumps = forced.clone();
        assumps.push(enc.plit(i));
        if let Some(m) = solve_verified(
            &mut solver,
            &enc,
            &assumps,
            &mut encoded,
            &mut lits,
            &mut sats,
        ) {
            forced = assumps;
            model = m;
        }
    }
    let mut selected = extract_needed(&enc, &model);

    // 精化:优先最新版本 —— 仅对"新装入"的键尝试强制最新非本地候选。
    // 已装包保持安装时不升级(apt 语义);对无关键强制选择会把整个仓库
    // 拉进安装集,且在真实规模(数万键)下产生海量重复求解。
    let kept_installed_keys: HashSet<String> = sys
        .installed
        .iter()
        .enumerate()
        .filter(|(i, _)| selected.contains(i))
        .map(|(_, p)| p.key.clone())
        .collect();
    let mut keys_touched: Vec<String> = selected
        .iter()
        .filter_map(|&i| {
            let c = i.checked_sub(enc.n_installed)?;
            (c < enc.n_cands).then(|| sys.all_candidates[c].pkg.key.clone())
        })
        .collect();
    keys_touched.sort();
    keys_touched.dedup();
    for key in keys_touched {
        if kept_installed_keys.contains(&key) {
            continue;
        }
        let Some(cands) = sys.candidates.get(&key) else {
            continue;
        };
        // add_candidates 已按版本降序排列;手工构造状态时按列表顺序
        let Some(&newest) = cands.iter().find(|&&c| !sys.all_candidates[c].is_local()) else {
            continue;
        };
        let newest_idx = newest + enc.n_installed;
        if selected.contains(&newest_idx) {
            continue;
        }
        let mut assumps = forced.clone();
        assumps.push(enc.plit(newest_idx));
        if let Some(m) = solve_verified(
            &mut solver,
            &enc,
            &assumps,
            &mut encoded,
            &mut lits,
            &mut sats,
        ) {
            forced = assumps;
            selected = extract_needed(&enc, &m);
        }
    }

    // 依赖驱动降级检查:apt/gdebi 不会为满足依赖自动把已装包降级。
    // 本地包自身的降级已由 resolve 前置检查拦截;这里拦住“依赖要求旧版本,
    // 求解器选了一个低于已装版本的仓库候选”的静默降级。
    for &idx in &selected {
        let Some(c) = idx.checked_sub(enc.n_installed) else {
            continue;
        };
        let cand = if c >= enc.n_cands {
            enc.local.expect("local unit present")
        } else {
            &sys.all_candidates[c]
        };
        if cand.is_local() {
            continue;
        }
        if let Some(installed) = sys.installed.iter().find(|p| p.key == cand.pkg.key)
            && cmp_version(&installed.version, &cand.pkg.version) == std::cmp::Ordering::Greater
        {
            return Verdict::Uninstallable(tf(
                "Installing {0} would require downgrading it from {1} to {2}",
                &[&cand.pkg.key, &installed.version, &cand.pkg.version],
            ));
        }
    }

    // 结果组装
    let mut changes = Changes::default();
    let mut downloads = Vec::new();
    let mut install_keys: HashSet<String> = Default::default();
    for &idx in &selected {
        if idx < enc.n_installed {
            continue;
        }
        let cand_ref: &crate::sysstate::Candidate = if idx - enc.n_installed >= enc.n_cands {
            enc.local.expect("local unit present")
        } else {
            &sys.all_candidates[idx - enc.n_installed]
        };
        if install_keys.insert(cand_ref.pkg.key.clone()) {
            changes.install.push(cand_ref.pkg.key.clone());
        }
        if let Some(repo) = &cand_ref.repo {
            let url = format!(
                "{}/{}",
                repo.base_uri.trim_end_matches('/'),
                cand_ref.repo_filename
            );
            if !repo.trusted {
                changes.unauthenticated.push(cand_ref.pkg.key.clone());
            }
            let fname = cand_ref
                .repo_filename
                .rsplit('/')
                .next()
                .unwrap_or("package.deb");
            downloads.push(DownloadSpec {
                name: cand_ref.pkg.key.clone(),
                version: cand_ref.pkg.version.clone(),
                url,
                dest: PathBuf::from(crate::download::CACHE_DIR).join(fname),
                sha256: cand_ref.sha256.clone(),
                size: cand_ref.size,
                trusted: repo.trusted,
            });
        }
    }
    for (i, pkg) in sys.installed.iter().enumerate() {
        // 被新候选替换(升级/重装)不算卸载
        if !selected.contains(&i) && !install_keys.contains(&pkg.key) {
            changes.remove.push(pkg.key.clone());
        }
    }
    changes.unauthenticated.sort();
    changes.unauthenticated.dedup();
    Verdict::Installable(Resolution { changes, downloads })
}

impl Encoding<'_> {
    fn plit_of(&self, u: Unit) -> Lit {
        let idx = match u {
            Unit::Installed(i) => i,
            Unit::Candidate(c) => self.n_installed + c,
        };
        self.plit(idx)
    }

    fn nlit_of(&self, u: Unit) -> Lit {
        let idx = match u {
            Unit::Installed(i) => i,
            Unit::Candidate(c) => self.n_installed + c,
        };
        self.nlit(idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysstate::{Candidate, Pkg, Repo, SysState};

    fn pkg(key: &str, ver: &str) -> Pkg {
        let name = base_name(key).to_string();
        let arch = key
            .split_once(':')
            .map(|(_, a)| a.to_string())
            .unwrap_or_else(|| "amd64".into());
        Pkg {
            key: key.into(),
            name,
            arch,
            version: ver.into(),
            ..Default::default()
        }
    }

    fn cand(key: &str, ver: &str, trusted: bool) -> Candidate {
        Candidate {
            pkg: pkg(key, ver),
            repo: Some(Repo {
                trusted,
                base_uri: "http://repo".into(),
                ..Default::default()
            }),
            repo_filename: format!("{key}_{ver}.deb"),
            ..Default::default()
        }
    }

    fn sys_with(pkgs: Vec<Pkg>) -> SysState {
        let mut st = SysState {
            native_arch: "amd64".into(),
            ..Default::default()
        };
        for p in pkgs {
            let i = st.installed.len();
            st.index.entry(p.key.clone()).or_insert(i);
            st.index
                .entry(format!("{}:{}", p.name, p.arch))
                .or_insert(i);
            st.index.entry(p.name.clone()).or_insert(i);
            st.installed.push(p);
        }
        st
    }

    fn local(key: &str, ver: &str) -> Candidate {
        Candidate {
            pkg: pkg(key, ver),
            local_path: Some("/tmp/x.deb".into()),
            ..Default::default()
        }
    }

    fn dep(name: &str, oper: &str, ver: &str) -> Vec<Vec<Dep>> {
        vec![vec![Dep {
            name: name.into(),
            ver: ver.into(),
            oper: oper.into(),
        }]]
    }

    fn verdict_ok(v: Verdict) -> Resolution {
        match v {
            Verdict::Installable(r) => r,
            Verdict::Uninstallable(m) => panic!("expected installable, got: {m}"),
        }
    }

    fn assert_uninstallable(v: Verdict) -> String {
        match v {
            Verdict::Uninstallable(m) => m,
            Verdict::Installable(_) => panic!("expected uninstallable"),
        }
    }

    #[test]
    fn simple_dep_resolves() {
        let mut st = sys_with(vec![pkg("libc6", "2.39")]);
        st.add_candidates(vec![cand("libfoo", "1.0", true)]);
        let mut l = local("myapp", "2.0");
        l.pkg.depends = dep("libfoo", "", "");
        let r = verdict_ok(encode_and_solve(&st, &l));
        assert!(r.changes.install.contains(&"libfoo".to_string()));
        assert!(r.changes.install.contains(&"myapp".to_string()));
        assert_eq!(r.downloads.len(), 1);
        assert_eq!(r.downloads[0].url, "http://repo/libfoo_1.0.deb");
        assert!(r.downloads[0].trusted);
    }

    #[test]
    fn or_group_picks_satisfiable_branch() {
        let mut st = sys_with(vec![]);
        st.add_candidates(vec![cand("pkg-b", "3.0", true)]);
        let mut l = local("myapp", "1.0");
        l.pkg.depends = vec![vec![
            Dep {
                name: "pkg-a".into(),
                ver: "2".into(),
                oper: ">=".into(),
            },
            Dep {
                name: "pkg-b".into(),
                ver: String::new(),
                oper: String::new(),
            },
        ]];
        let r = verdict_ok(encode_and_solve(&st, &l));
        assert!(r.changes.install.contains(&"pkg-b".to_string()));
        assert!(!r.changes.install.contains(&"pkg-a".to_string()));
    }

    #[test]
    fn versioned_dep_prefers_compatible() {
        let mut st = sys_with(vec![]);
        st.add_candidates(vec![cand("libx", "1.0", true), cand("libx", "2.0", true)]);
        let mut l = local("myapp", "1.0");
        l.pkg.depends = dep("libx", "<<", "1.5");
        let r = verdict_ok(encode_and_solve(&st, &l));
        assert_eq!(r.downloads[0].version, "1.0");
    }

    #[test]
    fn unsat_reports_uninstallable() {
        let st = sys_with(vec![]);
        let mut l = local("myapp", "1.0");
        l.pkg.depends = dep("nonexistent", "", "");
        assert_uninstallable(encode_and_solve(&st, &l));
    }

    #[test]
    fn conflict_forces_removal_of_non_essential() {
        let st = sys_with(vec![pkg("oldtool", "1.0")]);
        let mut l = local("newtool", "1.0");
        l.pkg.conflicts = dep("oldtool", "", "");
        let r = verdict_ok(encode_and_solve(&st, &l));
        assert!(r.changes.remove.contains(&"oldtool".to_string()));
    }

    #[test]
    fn essential_never_removed() {
        let st = sys_with(vec![Pkg {
            essential: true,
            ..pkg("core", "1.0")
        }]);
        let mut l = local("rival", "1.0");
        l.pkg.conflicts = dep("core", "", "");
        assert_uninstallable(encode_and_solve(&st, &l));
    }

    /// Protected: yes 与 Essential 同级:被本地包冲突时不得自动卸载
    /// (Debian Policy 5.6.11,dpkg --force-remove-protected 的守门对象)
    #[test]
    fn protected_never_removed() {
        let st = sys_with(vec![Pkg {
            protected: true,
            ..pkg("crypto", "1.0")
        }]);
        let mut l = local("rival", "1.0");
        l.pkg.conflicts = dep("crypto", "", "");
        assert_uninstallable(encode_and_solve(&st, &l));
    }

    /// 已装包 Breaks 版本命中的仓库直连候选:此前该对未编码,keep-all
    /// 快路径误判"可安装",违规延迟到 dpkg 解包阶段才暴露。编码后的
    /// 语义与 Conflicts 一致 —— 冲突方需在事务中先被卸载/保护则不可安装。
    #[test]
    fn installed_breaks_forces_removal_of_breaker() {
        let mut st = sys_with(vec![Pkg {
            breaks: dep("lsb-base", "<<", "3.3"),
            ..pkg("dpkg", "1.20")
        }]);
        st.add_candidates(vec![cand("lsb-base", "3.1", true)]);
        let mut l = local("myapp", "1.0");
        l.pkg.depends = dep("lsb-base", "", "");
        let r = verdict_ok(encode_and_solve(&st, &l));
        assert!(r.changes.remove.contains(&"dpkg".to_string()));
    }

    /// 真实 Debian 上 dpkg 本身 Essential: yes:候选受害方不得以卸载
    /// Essential 的代价被选中 → 不可安装
    #[test]
    fn installed_breaks_block_versioned_direct_candidate() {
        let mut st = sys_with(vec![Pkg {
            essential: true,
            breaks: dep("lsb-base", "<<", "3.3"),
            ..pkg("dpkg", "1.20")
        }]);
        st.add_candidates(vec![cand("lsb-base", "3.1", true)]);
        let mut l = local("myapp", "1.0");
        l.pkg.depends = dep("lsb-base", "", "");
        assert_uninstallable(encode_and_solve(&st, &l));
    }

    /// 同上场景改经虚拟包满足:无版本提供不受版本化 Breaks 制约(dpkg
    /// ≥1.17.11 语义),经提供者候选仍可满足依赖且不动已装保
    #[test]
    fn installed_breaks_miss_unversioned_candidate_provider() {
        let mut st = sys_with(vec![Pkg {
            breaks: dep("lsb-base", "<<", "3.3"),
            ..pkg("dpkg", "1.20")
        }]);
        st.add_candidates(vec![Candidate {
            pkg: Pkg {
                provides: vec![("lsb-base".into(), None)],
                ..pkg("sysvinit-utils", "3.0")
            },
            ..cand("sysvinit-utils", "3.0", true)
        }]);
        let mut l = local("myapp", "1.0");
        l.pkg.depends = dep("lsb-base", "", "");
        let r = verdict_ok(encode_and_solve(&st, &l));
        assert!(r.changes.remove.is_empty());
        assert!(r.changes.install.contains(&"sysvinit-utils".to_string()));
    }

    /// 候选×候选的 Breaks 互斥:同一变更集内两个新装包互 Breaking 时不可
    /// 同时选中(本地同时依赖两者 → 不可安装;仅依赖一方 → 正常安装)
    #[test]
    fn breaks_between_candidates_block_selection() {
        let mut st = sys_with(vec![]);
        st.add_candidates(vec![
            Candidate {
                pkg: Pkg {
                    breaks: dep("app-b", "", ""),
                    ..pkg("app-a", "1.0")
                },
                ..cand("app-a", "1.0", true)
            },
            cand("app-b", "1.0", true),
        ]);
        let mut l = local("myapp", "1.0");
        l.pkg.depends = vec![
            vec![Dep {
                name: "app-a".into(),
                ver: String::new(),
                oper: String::new(),
            }],
            vec![Dep {
                name: "app-b".into(),
                ver: String::new(),
                oper: String::new(),
            }],
        ];
        assert_uninstallable(encode_and_solve(&st, &l));

        let mut l2 = local("myapp2", "1.0");
        l2.pkg.depends = dep("app-a", "", "");
        let r = verdict_ok(encode_and_solve(&st, &l2));
        assert!(r.changes.install.contains(&"app-a".to_string()));
        assert!(!r.changes.install.contains(&"app-b".to_string()));
    }

    #[test]
    fn installed_package_satisfies_dep_without_download() {
        let st = sys_with(vec![pkg("libc6", "2.39")]);
        let mut l = local("myapp", "1.0");
        l.pkg.depends = dep("libc6", ">=", "2.30");
        let r = verdict_ok(encode_and_solve(&st, &l));
        assert!(r.downloads.is_empty());
        // required_changes 仍含本地包自身(变更集语义)
        assert_eq!(r.changes.install, vec!["myapp".to_string()]);
    }

    #[test]
    fn provides_satisfies_dep_when_no_direct_hit() {
        let mut st = sys_with(vec![]);
        st.add_candidates(vec![Candidate {
            pkg: Pkg {
                provides: vec![("www-browser".into(), None)],
                ..pkg("w3m", "0.5.3")
            },
            ..cand("w3m", "0.5.3", true)
        }]);
        let mut l = local("myapp", "1.0");
        l.pkg.depends = dep("www-browser", "", "");
        let r = verdict_ok(encode_and_solve(&st, &l));
        assert!(r.changes.install.contains(&"w3m".to_string()));
    }

    /// dpkg ≥1.17.11:版本化约束仅命中版本化提供 —— 已装包不得因
    /// "版本化 Breaks × 无版本提供"被误判冲突(否则 Essential×Essential
    /// 的假冲突会让真实系统上所有包误报不可安装)
    #[test]
    fn versioned_breaks_miss_unversioned_provider() {
        let st = sys_with(vec![
            Pkg {
                essential: true,
                breaks: dep("lsb-base", "<<", "3.3"),
                ..pkg("dpkg", "1.0")
            },
            Pkg {
                essential: true,
                provides: vec![("lsb-base".into(), None)],
                ..pkg("sysvinit-utils", "3.0")
            },
        ]);
        let r = verdict_ok(encode_and_solve(&st, &local("myapp", "1.0")));
        assert!(r.changes.remove.is_empty());
    }

    /// 版本化提供可满足版本化依赖;无版本提供不可
    #[test]
    fn versioned_dep_needs_versioned_provide() {
        let mut st = sys_with(vec![]);
        st.add_candidates(vec![Candidate {
            pkg: Pkg {
                provides: vec![("www-browser".into(), Some("1.0".into()))],
                ..pkg("w3m", "0.5.3")
            },
            ..cand("w3m", "0.5.3", true)
        }]);
        let mut l = local("myapp", "1.0");
        l.pkg.depends = dep("www-browser", ">=", "0.9");
        assert!(
            verdict_ok(encode_and_solve(&st, &l))
                .changes
                .install
                .contains(&"w3m".to_string())
        );
        // 仅无版本提供时不满足版本化依赖
        let mut st2 = sys_with(vec![]);
        st2.add_candidates(vec![Candidate {
            pkg: Pkg {
                provides: vec![("www-browser".into(), None)],
                ..pkg("w3m", "0.5.3")
            },
            ..cand("w3m", "0.5.3", true)
        }]);
        assert_uninstallable(encode_and_solve(&st2, &l));
    }

    #[test]
    fn unauth_repos_tracked() {
        let mut st = sys_with(vec![]);
        st.add_candidates(vec![cand("libfoo", "1.0", false)]);
        let mut l = local("myapp", "1.0");
        l.pkg.depends = dep("libfoo", "", "");
        let r = verdict_ok(encode_and_solve(&st, &l));
        assert_eq!(r.changes.unauthenticated, vec!["libfoo".to_string()]);
        assert!(!r.downloads[0].trusted);
    }

    #[test]
    fn reinstall_same_version_not_reported_as_removal() {
        let st = sys_with(vec![pkg("myapp", "1.0")]);
        let r = verdict_ok(encode_and_solve(&st, &local("myapp", "1.0")));
        assert!(r.changes.remove.is_empty());
        assert_eq!(r.changes.install, vec!["myapp".to_string()]);
    }

    /// ":any" 依赖只接受 Multi-Arch: allowed(dpkg 实测语义):已装
    /// python3:amd64 标记 allowed 时必须命中,否则 satisfiers 为空会生成
    /// 强制卸载的单元子句,keep-all 快路径 UNSAT 且连带误删依赖方
    #[test]
    fn any_arch_qualifier_matches_installed() {
        let st = sys_with(vec![Pkg {
            multi_arch: "allowed".into(),
            ..pkg("python3", "3.12")
        }]);
        let mut l = local("app", "1.0");
        l.pkg.depends = dep("python3:any", "", "");
        let r = verdict_ok(encode_and_solve(&st, &l));
        assert!(r.downloads.is_empty());
        assert_eq!(r.changes.install, vec!["app".to_string()]);
        assert!(r.changes.remove.is_empty());
    }

    /// ":any" 不接受仅 Multi-Arch: foreign/same 或无标记的包
    #[test]
    fn any_arch_requires_multi_arch_allowed() {
        for ma in ["", "foreign", "same", "no"] {
            let st = sys_with(vec![Pkg {
                multi_arch: ma.into(),
                ..pkg("python3", "3.12")
            }]);
            let mut l = local("app", "1.0");
            l.pkg.depends = dep("python3:any", "", "");
            assert_uninstallable(encode_and_solve(&st, &l));
        }
    }

    /// Essential/Protected 升级与重装:保护约束是“同 key 仍有包存在”,
    /// 而不是“旧的已装单元必须为真”。后者会与 AMO + 本地强制选择冲突,
    /// 把 bash/dpkg 升级误报为不可安装。
    #[test]
    fn essential_same_key_upgrade_and_reinstall_installable() {
        for (installed, local_ver) in [("5.2", "5.3"), ("5.2", "5.2")] {
            let st = sys_with(vec![Pkg {
                essential: true,
                ..pkg("bash", installed)
            }]);
            let r = verdict_ok(encode_and_solve(&st, &local("bash", local_ver)));
            assert_eq!(r.changes.install, vec!["bash".to_string()]);
            assert!(r.changes.remove.is_empty());
        }
    }

    #[test]
    fn protected_same_key_upgrade_installable() {
        let st = sys_with(vec![Pkg {
            protected: true,
            ..pkg("crypto", "1.0")
        }]);
        let r = verdict_ok(encode_and_solve(&st, &local("crypto", "2.0")));
        assert_eq!(r.changes.install, vec!["crypto".to_string()]);
        assert!(r.changes.remove.is_empty());
    }

    /// Replaces 不豁免 Conflicts/Breaks:dpkg 会卸载被替换包并反配置其
    /// 反向依赖,而不是允许两者共存。旧豁免会漏报 remove,反向依赖被
    /// 误判满足,最终 dpkg -i 失败。
    #[test]
    fn replaces_does_not_exempt_conflict() {
        let st = sys_with(vec![
            pkg("oldtool", "1.0"),
            Pkg {
                depends: dep("oldtool", "", ""),
                ..pkg("dependent", "1.0")
            },
        ]);
        let mut l = local("newtool", "1.0");
        l.pkg.conflicts = dep("oldtool", "", "");
        l.pkg.replaces = dep("oldtool", "", "");
        let r = verdict_ok(encode_and_solve(&st, &l));
        assert!(r.changes.remove.contains(&"oldtool".to_string()));
        assert!(r.changes.remove.contains(&"dependent".to_string()));
    }

    /// 无限定依赖接受 Multi-Arch: foreign 的异架构包
    #[test]
    fn unqualified_dep_accepts_multi_arch_foreign() {
        let st = sys_with(vec![Pkg {
            multi_arch: "foreign".into(),
            ..pkg("foo:i386", "1.0")
        }]);
        let mut l = local("app", "1.0");
        l.pkg.depends = dep("foo", "", "");
        let r = verdict_ok(encode_and_solve(&st, &l));
        assert!(r.downloads.is_empty());
    }

    /// 无 Multi-Arch: foreign 标记的异架构包不满足无限定依赖
    #[test]
    fn unqualified_dep_rejects_foreign_without_multi_arch() {
        let st = sys_with(vec![pkg("foo:i386", "1.0")]);
        let mut l = local("app", "1.0");
        l.pkg.depends = dep("foo", "", "");
        assert_uninstallable(encode_and_solve(&st, &l));
    }

    /// 显式本机架构接受 arch:all;`:native` 按字面架构名处理,不命中
    /// (dpkg 二进制依赖只支持真实架构名与 `:any`)
    #[test]
    fn native_arch_qualifier_and_arch_all() {
        let st = sys_with(vec![Pkg {
            arch: "all".into(),
            ..pkg("data", "1.0")
        }]);
        let mut explicit = local("app", "1.0");
        explicit.pkg.depends = dep("data:amd64", "", "");
        assert!(
            verdict_ok(encode_and_solve(&st, &explicit))
                .downloads
                .is_empty()
        );

        let mut native = local("app2", "1.0");
        native.pkg.depends = dep("data:native", "", "");
        assert_uninstallable(encode_and_solve(&st, &native));

        // 即使目标包就是本机架构,`:native` 也不命中
        let native_st = sys_with(vec![pkg("foo", "1.0")]);
        let mut literal = local("app3", "1.0");
        literal.pkg.depends = dep("foo:native", "", "");
        assert_uninstallable(encode_and_solve(&native_st, &literal));
    }

    /// 依赖要求旧版本时不得静默降级已装包(apt/gdebi 同样拒绝)
    #[test]
    fn dependency_driven_downgrade_rejected() {
        let mut st = sys_with(vec![pkg("libx", "2.0")]);
        st.add_candidates(vec![cand("libx", "1.0", true)]);
        let mut l = local("app", "1.0");
        l.pkg.depends = dep("libx", "<<", "2.0");
        let msg = assert_uninstallable(encode_and_solve(&st, &l));
        assert!(msg.contains("downgrading"), "unexpected message: {msg}");
    }

    #[test]
    fn ver_matches_operators() {
        assert!(ver_matches("2.39", ">=", "2.30"));
        assert!(!ver_matches("1.0", "<<", "1.0"));
        assert!(ver_matches("1.0", "=", "1.0"));
        assert!(ver_matches("1.0", ">", "0.9"));
    }

    /// 无关的仓库候选不得被拉进安装集(精化只作用于已选中的键;
    /// 曾在真实规模上把整个仓库标记为待安装并产生海量重复求解)
    #[test]
    fn unrelated_repo_candidates_not_installed() {
        let mut st = sys_with(vec![]);
        let cands: Vec<Candidate> = (0..200)
            .map(|i| cand(&format!("unrel-{i}"), "1.0", true))
            .collect();
        st.add_candidates(cands);
        let r = verdict_ok(encode_and_solve(&st, &local("myapp", "1.0")));
        assert_eq!(r.changes.install, vec!["myapp".to_string()]);
        assert!(r.downloads.is_empty());
    }

    /// 与安装无关的已装包必须保持(keep-all 快路径;曾因默认假值模型
    /// 把数千已装包误报为"待卸载")
    #[test]
    fn unrelated_installed_packages_kept() {
        let st = sys_with(vec![pkg("unused-a", "1.0"), pkg("unused-b", "2.0")]);
        let r = verdict_ok(encode_and_solve(&st, &local("myapp", "1.0")));
        assert!(r.changes.remove.is_empty());
        assert_eq!(r.changes.install, vec!["myapp".to_string()]);
    }

    /// 版本门槛强制换版:已装旧版被新候选替换,替换不算卸载
    #[test]
    fn version_gate_replaces_installed() {
        let mut st = sys_with(vec![pkg("libx", "1.0")]);
        st.add_candidates(vec![cand("libx", "2.0", true)]);
        let mut l = local("myapp", "1.0");
        l.pkg.depends = dep("libx", ">=", "2.0");
        let r = verdict_ok(encode_and_solve(&st, &l));
        assert!(r.changes.install.contains(&"libx".to_string()));
        assert!(r.changes.remove.is_empty());
    }

    /// 冲突编码按名字索引取 victim:候选带冲突项时复杂度与命中数相关,
    /// 而非单元总数平方(真实索引规模下曾是分钟级卡死)
    #[test]
    fn conflicts_encoding_scales_by_name_index() {
        let mut st = sys_with(vec![]);
        let cands: Vec<Candidate> = (0..500)
            .map(|i| {
                let mut c = cand(&format!("c-{i}"), "1.0", true);
                c.pkg.conflicts = dep("definitely-not-installed", "", "");
                c
            })
            .collect();
        st.add_candidates(cands);
        let r = verdict_ok(encode_and_solve(&st, &local("myapp", "1.0")));
        assert_eq!(r.changes.install, vec!["myapp".to_string()]);
    }
}
