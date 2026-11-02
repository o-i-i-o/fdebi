//! varisat(CDCL SAT)编码的依赖求解器。
//!
//! 求解域:已安装库(可卸载)∪ 仓库候选 ∪ 本地 deb(强制)。
//! 编码:
//! - 变量 = 选择单元:已安装包"保持安装"或某候选版本"被选中"
//! - 同名二进制包(键)单元两两互斥(AMO)
//! - Depends/Pre-Depends:选中 → 每个依赖组至少一个满足者(直接命中或虚拟提供)
//! - Conflicts/Breaks:互斥子句;Replaces 覆盖时豁免(dpkg 语义)
//! - Essential 包强制保持安装(保护性约束)
//!
//! 求解策略:已装包默认全部保持(apt 语义快路径)→ 必要时退回假设式
//! 迭代精化(最小化卸载、新装键优先新版本)。
//!
//! 失败原因统一为"求解失败"类消息(不逐条罗列);or 组解析交给 SAT
//! 而非首分支回溯。

use std::collections::{BTreeMap, HashSet};
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
    /// 本地 deb(独立单元,排在候选之后;不落入 sys 的缓存对象)
    local: Option<crate::sysstate::Candidate>,
    n_installed: usize,
    n_cands: usize,
    providers: BTreeMap<String, Vec<Unit>>,
    by_name: BTreeMap<String, Vec<Unit>>,
    /// 键 → 单元(AMO 分组)
    groups: BTreeMap<String, Vec<Unit>>,
}

impl<'a> Encoding<'a> {
    fn new(sys: &'a SysState, local: Option<crate::sysstate::Candidate>) -> Self {
        let n_installed = sys.installed.len();
        let n_cands = sys.all_candidates.len();
        let mut providers: BTreeMap<String, Vec<Unit>> = BTreeMap::new();
        let mut by_name: BTreeMap<String, Vec<Unit>> = BTreeMap::new();
        let mut groups: BTreeMap<String, Vec<Unit>> = BTreeMap::new();
        let mut push = |u: Unit, pkg: &Pkg| {
            by_name.entry(pkg.name.clone()).or_default().push(u);
            for p in &pkg.provides {
                providers.entry(p.0.clone()).or_default().push(u);
            }
            groups.entry(pkg.key.clone()).or_default().push(u);
        };
        for (i, pkg) in sys.installed.iter().enumerate() {
            push(Unit::Installed(i), pkg);
        }
        for (c, cand) in sys.all_candidates.iter().enumerate() {
            push(Unit::Candidate(c), &cand.pkg);
        }
        if let Some(local) = &local {
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
                    &self.local.as_ref().expect("local unit present").pkg
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
    /// 版本化约束经虚拟提供命中时要求版本化提供(dpkg ≥1.17.11 语义)
    fn satisfiers(&self, d: &Dep) -> Vec<Unit> {
        let mut out = Vec::new();
        self.satisfiers_into(d, &mut out);
        out
    }

    /// 同 satisfiers,复用调用方缓冲(依赖子句生成在真实索引规模下
    /// 调用数十万次,逐次分配是 resolve 的主要开销)
    fn satisfiers_into(&self, d: &Dep, out: &mut Vec<Unit>) {
        out.clear();
        if let Some(units) = self.by_name.get(base_name(&d.name)) {
            out.extend(units.iter().copied().filter(|&u| {
                let pkg = self.meta(u);
                dep_covers(&d.name, pkg, self.sys.native_arch.as_str())
                    && ver_matches(&pkg.version, &d.oper, &d.ver)
            }));
        }
        if let Some(units) = self.providers.get(base_name(&d.name)) {
            out.extend(
                units
                    .iter()
                    .copied()
                    .filter(|&u| self.provide_ver_matches(u, d)),
            );
        }
    }

    /// 提供命中时 u 是否满足 d 的版本约束(无版本约束恒真;
    /// 有版本约束时必须存在匹配的版本化提供)
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

    /// U 的 Conflicts/Breaks 条目 c 是否命中单元 v
    fn hits(&self, c: &Dep, v: Unit) -> bool {
        let pkg = self.meta(v);
        if dep_covers(&c.name, pkg, self.sys.native_arch.as_str()) {
            c.ver.is_empty() || ver_matches(&pkg.version, &c.oper, &c.ver)
        } else if self
            .providers
            .get(base_name(&c.name))
            .is_some_and(|ps| ps.contains(&v))
        {
            // 提供命中:版本化约束仅适用版本化提供
            // (否则 dpkg 的版本化 Breaks 会误伤无版本提供者,如
            //  dpkg Breaks lsb-base (<< x) 对 sysvinit-utils 的无版本提供)
            self.provide_ver_matches(v, c)
        } else {
            false
        }
    }

    /// U 的 Replaces 是否覆盖 v(冲突豁免)
    fn replaces_covers(&self, u: Unit, v: Unit) -> bool {
        let vpkg = self.meta(v);
        self.meta(u).replaces.iter().any(|g| {
            g.iter().any(|r| {
                dep_covers(&r.name, vpkg, self.sys.native_arch.as_str())
                    && (r.ver.is_empty() || ver_matches(&vpkg.version, &r.oper, &r.ver))
            })
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
        lits.clear();
        lits.push(self.nlit(idx));
        for d in group {
            sats.clear();
            self.satisfiers_into(d, sats);
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

    /// 组是否已在模型内被满足(存在满足者为真)
    fn group_satisfied_in_model(&self, group: &[Dep], model: &HashSet<usize>) -> bool {
        group.iter().any(|d| {
            self.satisfiers(d)
                .iter()
                .any(|&s| model.contains(&self.unit_idx(s)))
        })
    }
}

/// 添加子句(排序去重,空子句即 UNSAT 由求解器报告);复用调用方缓冲
fn add_clause(solver: &mut Solver, lits: &mut Vec<Lit>) {
    lits.sort();
    lits.dedup();
    solver.add_clause(lits);
}

/// 依赖名是否覆盖给定包名(考虑 :arch 限定)。
/// dpkg 语义:无限定匹配任意架构;":any" 匹配任意架构;":native"
/// 仅本机架构;其余限定精确匹配架构 —— 曾把 ":any" 当字面架构名,
/// 真实系统上 python3:any/perl:any 类依赖 satisfiers 为空,生成
/// 强制卸载已装包的单元子句,导致 keep-all 快路径 UNSAT
fn dep_covers(dep_name: &str, pkg: &Pkg, native_arch: &str) -> bool {
    match dep_name.split_once(':') {
        Some((base, arch)) => {
            pkg.name == base
                && match arch {
                    "any" => true,
                    "native" => pkg.arch == native_arch,
                    a => pkg.arch == a,
                }
        }
        None => pkg.name == dep_name,
    }
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

    Ok(encode_and_solve(
        sys,
        local_candidate(deb, &sys.native_arch),
    ))
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
fn extract_needed(enc: &Encoding<'_>, model: &HashSet<usize>) -> HashSet<usize> {
    // 已装保持集:模型真值中的已装单元全部保留(不参与可达性裁剪)
    let mut needed: HashSet<usize> = (0..enc.n_installed).filter(|i| model.contains(i)).collect();
    if enc.local.is_some() {
        needed.insert(enc.n_installed + enc.n_cands);
    }
    let mut queue: Vec<usize> = needed.iter().copied().collect();
    while let Some(idx) = queue.pop() {
        let meta = enc.meta(enc.unit(idx));
        for group in meta.depends.iter().chain(meta.pre_depends.iter()) {
            // 组已被必要集满足则不新增(模型可能冗余置真其他满足者)
            if group.iter().any(|d| {
                enc.satisfiers(d)
                    .iter()
                    .any(|&s| needed.contains(&enc.unit_idx(s)))
            }) {
                continue;
            }
            // 组未被满足:按 satisfiers 顺序取首个模型置真的满足者
            for d in group {
                let chosen = enc
                    .satisfiers(d)
                    .into_iter()
                    .map(|s| enc.unit_idx(s))
                    .find(|&s_idx| model.contains(&s_idx));
                if let Some(s_idx) = chosen {
                    if needed.insert(s_idx) {
                        queue.push(s_idx);
                    }
                    break;
                }
            }
        }
    }
    needed
}

/// 假设求解 + 模型验证(依赖子句惰性编码的收敛循环)。
///
/// 子句库只含已装单元与本地 deb 的全部依赖子句;仓库候选的依赖子句按需
/// 补加:每轮检查"模型真值单元的每个依赖组在模型内是否有满足者",违反者
/// 补加其子句后重解(varisat 增量接口,已学子句保留)。
/// - 返回 None:当前子句库在假设下 UNSAT —— 子句库 ⊆ 全公式,子句集越小
///   可满足性越宽松,故全公式必 UNSAT,结论可直接采信;
/// - 返回 Some:验证通过时模型满足全公式(假值单元的依赖子句被 ¬u 平凡
///   满足),必要集已就地提取。
///
/// 验证按单元编号升序扫描,同一输入的补加次序确定,结果可复现。
fn solve_verified(
    solver: &mut Solver,
    enc: &Encoding<'_>,
    forced: &[Lit],
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
            let meta = enc.meta(enc.unit(idx));
            for group in meta.depends.iter().chain(meta.pre_depends.iter()) {
                if enc.group_satisfied_in_model(group, &model) {
                    continue;
                }
                enc.dep_group_clause(idx, group, solver, lits, sats);
                added = true;
            }
        }
        if !added {
            return Some(extract_needed(enc, &model));
        }
    }
}

fn encode_and_solve(sys: &SysState, local: crate::sysstate::Candidate) -> Verdict {
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
    // 的守门对象;Protected: yes 同样包括 dpkg 兼容的旧写法 Important: yes)
    for (i, pkg) in sys.installed.iter().enumerate() {
        if pkg.essential || pkg.protected {
            lits.clear();
            lits.push(enc.plit(i));
            add_clause(&mut solver, &mut lits);
        }
    }

    // 同键互斥(AMO)
    for group in enc.groups.values() {
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

    // Conflicts 与 Breaks(按名字索引取 victim,避免全单元平方级扫描)。
    // 两者对 solver 同构(apt 解析语义:breaks 也是互斥约束),作用于全部单元
    // —— 已装×候选(如已装 dpkg Breaks 旧版 lsb-base,候选 lsb-base 不得
    // 被选中降 dead dpkg)与候选×候选(同一变更集内互不相容)都必须编码,
    // 否则违规要到 dpkg 配置阶段(--auto-deconfigure 会把已装受害方降置
    // deconfigured)才暴露。仅"双方均已安装"的存量违规不回溯修复:
    // dpkg 不会重新校验存量态,历史冲突不应当阻断 keep-all 快路径。
    for idx in 0..n_units {
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
                    if enc.hits(c, v) && !enc.replaces_covers(u, v) {
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
    // 真实系统上已装数千包,若从默认假值模型出发逐包精化会产生数千次求解。
    let keeps: Vec<Lit> = (0..enc.n_installed).map(|i| enc.plit(i)).collect();
    let mut forced: Vec<Lit> = Vec::new();
    let mut selected;
    if let Some(sel) = solve_verified(&mut solver, &enc, &keeps, &mut lits, &mut sats) {
        selected = sel;
    } else {
        // 存在必须卸载/换版的包:退回无保留假设,再逐包尝试保持(最小化卸载)
        let mut sel = match solve_verified(&mut solver, &enc, &forced, &mut lits, &mut sats) {
            Some(sel) => sel,
            None => {
                return Verdict::Uninstallable(gettext(
                    "Failed to satisfy all dependencies (conflicting requests or missing packages)",
                ));
            }
        };
        for i in 0..enc.n_installed {
            if sel.contains(&i) {
                continue;
            }
            let mut assumps = forced.clone();
            assumps.push(enc.plit(i));
            if let Some(s) = solve_verified(&mut solver, &enc, &assumps, &mut lits, &mut sats) {
                forced = assumps;
                sel = s;
            }
        }
        selected = sel;
    }

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
        if let Some(sel) = solve_verified(&mut solver, &enc, &assumps, &mut lits, &mut sats) {
            forced = assumps;
            selected = sel;
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
            enc.local.as_ref().expect("local unit present")
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
        let r = verdict_ok(encode_and_solve(&st, l));
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
        let r = verdict_ok(encode_and_solve(&st, l));
        assert!(r.changes.install.contains(&"pkg-b".to_string()));
        assert!(!r.changes.install.contains(&"pkg-a".to_string()));
    }

    #[test]
    fn versioned_dep_prefers_compatible() {
        let mut st = sys_with(vec![]);
        st.add_candidates(vec![cand("libx", "1.0", true), cand("libx", "2.0", true)]);
        let mut l = local("myapp", "1.0");
        l.pkg.depends = dep("libx", "<<", "1.5");
        let r = verdict_ok(encode_and_solve(&st, l));
        assert_eq!(r.downloads[0].version, "1.0");
    }

    #[test]
    fn unsat_reports_uninstallable() {
        let st = sys_with(vec![]);
        let mut l = local("myapp", "1.0");
        l.pkg.depends = dep("nonexistent", "", "");
        assert_uninstallable(encode_and_solve(&st, l));
    }

    #[test]
    fn conflict_forces_removal_of_non_essential() {
        let st = sys_with(vec![pkg("oldtool", "1.0")]);
        let mut l = local("newtool", "1.0");
        l.pkg.conflicts = dep("oldtool", "", "");
        let r = verdict_ok(encode_and_solve(&st, l));
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
        assert_uninstallable(encode_and_solve(&st, l));
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
        assert_uninstallable(encode_and_solve(&st, l));
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
        let r = verdict_ok(encode_and_solve(&st, l));
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
        assert_uninstallable(encode_and_solve(&st, l));
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
        let r = verdict_ok(encode_and_solve(&st, l));
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
        assert_uninstallable(encode_and_solve(&st, l));

        let mut l2 = local("myapp2", "1.0");
        l2.pkg.depends = dep("app-a", "", "");
        let r = verdict_ok(encode_and_solve(&st, l2));
        assert!(r.changes.install.contains(&"app-a".to_string()));
        assert!(!r.changes.install.contains(&"app-b".to_string()));
    }

    #[test]
    fn installed_package_satisfies_dep_without_download() {
        let st = sys_with(vec![pkg("libc6", "2.39")]);
        let mut l = local("myapp", "1.0");
        l.pkg.depends = dep("libc6", ">=", "2.30");
        let r = verdict_ok(encode_and_solve(&st, l));
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
        let r = verdict_ok(encode_and_solve(&st, l));
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
        let r = verdict_ok(encode_and_solve(&st, local("myapp", "1.0")));
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
            verdict_ok(encode_and_solve(&st, l.clone()))
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
        assert_uninstallable(encode_and_solve(&st2, l));
    }

    #[test]
    fn unauth_repos_tracked() {
        let mut st = sys_with(vec![]);
        st.add_candidates(vec![cand("libfoo", "1.0", false)]);
        let mut l = local("myapp", "1.0");
        l.pkg.depends = dep("libfoo", "", "");
        let r = verdict_ok(encode_and_solve(&st, l));
        assert_eq!(r.changes.unauthenticated, vec!["libfoo".to_string()]);
        assert!(!r.downloads[0].trusted);
    }

    #[test]
    fn reinstall_same_version_not_reported_as_removal() {
        let st = sys_with(vec![pkg("myapp", "1.0")]);
        let r = verdict_ok(encode_and_solve(&st, local("myapp", "1.0")));
        assert!(r.changes.remove.is_empty());
        assert_eq!(r.changes.install, vec!["myapp".to_string()]);
    }

    /// ":any" 架构限定匹配任意架构(dpkg 语义):已装包的 python3:any
    /// 类依赖必须命中已装 python3:amd64,否则 satisfiers 为空会生成
    /// 强制卸载的单元子句,keep-all 快路径 UNSAT 且连带误删依赖方
    #[test]
    fn any_arch_qualifier_matches_installed() {
        let st = sys_with(vec![pkg("python3", "3.12")]);
        let mut l = local("app", "1.0");
        l.pkg.depends = dep("python3:any", "", "");
        let r = verdict_ok(encode_and_solve(&st, l));
        assert!(r.downloads.is_empty());
        assert_eq!(r.changes.install, vec!["app".to_string()]);
        assert!(r.changes.remove.is_empty());
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
        let r = verdict_ok(encode_and_solve(&st, local("myapp", "1.0")));
        assert_eq!(r.changes.install, vec!["myapp".to_string()]);
        assert!(r.downloads.is_empty());
    }

    /// 与安装无关的已装包必须保持(keep-all 快路径;曾因默认假值模型
    /// 把数千已装包误报为"待卸载")
    #[test]
    fn unrelated_installed_packages_kept() {
        let st = sys_with(vec![pkg("unused-a", "1.0"), pkg("unused-b", "2.0")]);
        let r = verdict_ok(encode_and_solve(&st, local("myapp", "1.0")));
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
        let r = verdict_ok(encode_and_solve(&st, l));
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
        let r = verdict_ok(encode_and_solve(&st, local("myapp", "1.0")));
        assert_eq!(r.changes.install, vec!["myapp".to_string()]);
    }
}
