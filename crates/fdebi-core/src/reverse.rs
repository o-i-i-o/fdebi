//! 卸载前的只读反向依赖检查。
//!
//! 对应 deepin `packageReverseDependsList`,但只做告警、不执行删除。

use std::collections::HashSet;

use crate::sysstate::{Dep, Pkg, SysState, base_name};
use crate::version::cmp_version;

/// 一条反向依赖告警:卸载某包会破坏 `package` 的依赖组 `requires`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReverseDepend {
    pub package: String,
    /// 依赖组原文(备选以 " | " 分隔)
    pub requires: String,
}

/// 反向依赖预检(只读)。
///
/// 仅检查 Depends/Pre-Depends(忽略 Recommends/Suggests,与 dpkg 阻止卸载的
/// 关系一致)。只有"移除前由待卸载包满足、移除后其余已装包都无法满足"的
/// 依赖组才算本次卸载引入的破坏;系统本来就未满足的依赖不报告。
pub fn reverse_dependents(sys: &SysState, removed: &[String]) -> Vec<ReverseDepend> {
    let removed_pkgs: Vec<&Pkg> = sys
        .installed
        .iter()
        .filter(|p| {
            removed
                .iter()
                .any(|r| r == &p.key || r == &p.name || r == &format!("{}:{}", p.name, p.arch))
        })
        .collect();
    if removed_pkgs.is_empty() {
        return Vec::new();
    }
    let removed_keys: HashSet<&str> = removed_pkgs.iter().map(|p| p.key.as_str()).collect();
    let remaining: Vec<&Pkg> = sys
        .installed
        .iter()
        .filter(|p| !removed_keys.contains(p.key.as_str()))
        .collect();
    let native = sys.native_arch.as_str();
    let satisfied = |group: &[Dep], pool: &[&Pkg]| {
        group
            .iter()
            .any(|d| pool.iter().any(|p| pkg_satisfies(p, d, &p.arch, native)))
    };
    let mut out: Vec<ReverseDepend> = Vec::new();
    for pkg in &remaining {
        for group in pkg.depends.iter().chain(pkg.pre_depends.iter()) {
            if satisfied(group, &remaining) {
                continue;
            }
            // 移除前该组只由待卸载包满足 → 本次卸载会把它变成未满足
            let was_satisfied = group.iter().any(|d| {
                removed_pkgs
                    .iter()
                    .any(|p| pkg_satisfies(p, d, &pkg.arch, native))
            });
            if was_satisfied {
                out.push(ReverseDepend {
                    package: pkg.name.clone(),
                    requires: group
                        .iter()
                        .map(|d| d.name.clone())
                        .collect::<Vec<_>>()
                        .join(" | "),
                });
            }
        }
    }
    out.sort_by(|a, b| {
        a.package
            .cmp(&b.package)
            .then_with(|| a.requires.cmp(&b.requires))
    });
    out.dedup();
    out
}

/// 依赖名(含 :arch 限定)是否命中包的 name 与架构
fn dep_name_matches(dep_name: &str, pkg: &Pkg, depender_arch: &str, native_arch: &str) -> bool {
    pkg.name == base_name(dep_name) && dep_arch_matches(dep_name, pkg, depender_arch, native_arch)
}

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

/// 单个包是否满足依赖表达式(直接同名或虚拟提供,含架构/版本约束)
fn pkg_satisfies(pkg: &Pkg, d: &Dep, depender_arch: &str, native_arch: &str) -> bool {
    if dep_name_matches(&d.name, pkg, depender_arch, native_arch)
        && ver_matches(&pkg.version, &d.oper, &d.ver)
    {
        return true;
    }
    if !dep_arch_matches(&d.name, pkg, depender_arch, native_arch) {
        return false;
    }
    pkg.provides.iter().any(|(n, pv)| {
        n == base_name(&d.name)
            && (d.ver.is_empty()
                || d.oper.is_empty()
                || pv
                    .as_ref()
                    .is_some_and(|pv| ver_matches(pv, &d.oper, &d.ver)))
    })
}
