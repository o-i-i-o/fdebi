//! 批量安装的纯逻辑:按本地 deb 之间的 Depends/Pre-Depends 关系排定顺序。
//!
//! 逐包执行(而非一次 `dpkg -i` 全部)才能做到失败隔离与逐包状态;
//! 但若 A 依赖同批的 B,必须先把 B 装进系统,求解器处理 A 时才能看到
//! 满足者。这里按依赖图拓扑排序,同层保持用户选择顺序;环按输入顺序
//! 追加(交给 dpkg 报错,不做猜测)。

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};

use crate::sysstate::{base_name, parse_depends, provided_names};

/// 批量条目参与排序的 control 字段(空串表示无)
pub struct LocalMeta<'a> {
    pub name: &'a str,
    pub depends: &'a str,
    pub pre_depends: &'a str,
    pub provides: &'a str,
}

/// 返回安装顺序(输入下标):被依赖者在前,依赖者在其后
pub fn order_by_dependencies(metas: &[LocalMeta<'_>]) -> Vec<usize> {
    let n = metas.len();

    // 名字(含 Provides 虚拟名)→ 条目下标;同名取先出现者
    let mut by_name: HashMap<String, usize> = HashMap::new();
    for (i, m) in metas.iter().enumerate() {
        by_name.entry(m.name.to_string()).or_insert(i);
        for (provided, _) in provided_names(m.provides) {
            by_name.entry(provided).or_insert(i);
        }
    }

    // 边:被依赖者 → 依赖者
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut indeg = vec![0usize; n];
    for (i, m) in metas.iter().enumerate() {
        let mut seen = HashSet::new();
        for group in parse_depends(m.depends)
            .into_iter()
            .chain(parse_depends(m.pre_depends))
        {
            for dep in group {
                let Some(&dep_idx) = by_name.get(base_name(&dep.name)) else {
                    continue;
                };
                if dep_idx != i && seen.insert(dep_idx) {
                    succ[dep_idx].push(i);
                    indeg[i] += 1;
                }
            }
        }
    }

    // Kahn 拓扑排序;最小堆保证同层按输入顺序
    let mut ready: BinaryHeap<Reverse<usize>> =
        (0..n).filter(|&i| indeg[i] == 0).map(Reverse).collect();
    let mut order = Vec::with_capacity(n);
    while let Some(Reverse(i)) = ready.pop() {
        order.push(i);
        for &next in &succ[i] {
            indeg[next] -= 1;
            if indeg[next] == 0 {
                ready.push(Reverse(next));
            }
        }
    }

    if order.len() < n {
        let mut placed = vec![false; n];
        for &i in &order {
            placed[i] = true;
        }
        order.extend((0..n).filter(|&i| !placed[i]));
    }
    order
}

#[cfg(test)]
mod tests {
    use super::{LocalMeta, order_by_dependencies};

    fn meta<'a>(name: &'a str, depends: &'a str) -> LocalMeta<'a> {
        LocalMeta {
            name,
            depends,
            pre_depends: "",
            provides: "",
        }
    }

    #[test]
    fn dependencies_come_first() {
        let metas = [
            meta("app", "libfoo (>= 1.0)"),
            meta("libfoo", ""),
            meta("tool", "app | other"),
        ];
        // app 依赖 libfoo;tool 的 or 组先匹配批内的 app
        assert_eq!(order_by_dependencies(&metas), vec![1, 0, 2]);
    }

    #[test]
    fn unrelated_keep_input_order() {
        let metas = [meta("b", ""), meta("a", "")];
        assert_eq!(order_by_dependencies(&metas), vec![0, 1]);
    }

    #[test]
    fn provides_satisfy_dependencies() {
        let mut provider = meta("provider", "");
        provider.provides = "virtual-api (= 2)";
        let metas = [meta("consumer", "virtual-api"), provider];
        assert_eq!(order_by_dependencies(&metas), vec![1, 0]);
    }

    #[test]
    fn cycles_fall_back_to_input_order() {
        let metas = [meta("a", "b"), meta("b", "a")];
        assert_eq!(order_by_dependencies(&metas), vec![0, 1]);
    }

    #[test]
    fn pre_depends_also_ordered() {
        let consumer = LocalMeta {
            name: "consumer",
            depends: "",
            pre_depends: "base",
            provides: "",
        };
        let metas = [consumer, meta("base", "")];
        assert_eq!(order_by_dependencies(&metas), vec![1, 0]);
    }
}
