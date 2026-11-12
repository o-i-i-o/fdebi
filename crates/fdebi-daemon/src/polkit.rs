//! Polkit 授权(安全守门人):守护进程执行特权操作前向 PolicyKit1 发起
//! CheckAuthorization;主体为发起 D-Bus 调用的总线名,交互式认证由
//! 用户会话中的 polkit agent(图形密码框)呈现。
//!
//! 策略文件:data/polkit/org.fdebi.policy(analyze 免授权,由方法
//! 本身的只读性质保证;install/remove 需 auth_admin_keep)。

use std::collections::HashMap;

use zbus::Connection;
use zbus::zvariant::Value;

const AUTHORITY: &str = "org.freedesktop.PolicyKit1.Authority";
const AUTHORITY_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";

/// 校验调用者是否被授权执行 action(AllowUserInteraction)
pub async fn check_authorized(
    conn: &Connection,
    caller_unique_name: &str,
    action: &str,
) -> zbus::Result<bool> {
    let authority = zbus::Proxy::new(
        conn,
        "org.freedesktop.PolicyKit1",
        AUTHORITY_PATH,
        AUTHORITY,
    )
    .await?;
    // Subject:(kind, a{sv});system-bus-name 以唯一名标识
    let subject: (String, HashMap<String, Value>) = (
        "system-bus-name".to_string(),
        HashMap::from([("name".to_string(), Value::from(caller_unique_name))]),
    );
    let details: HashMap<String, String> = HashMap::new();
    // Flags:1 = AllowUserInteraction(密码框可弹出)
    // 返回契约为 (is_authorized, is_challenge, details) 三元组 —— 曾按
    // (bool, a{ss}) 二元组反序列化,真实系统上每次 CheckAuthorization 都
    // 以 Signature mismatch 失败,安装事务从未走到下载/dpkg
    let (authorized, _challenge, _details): (bool, bool, HashMap<String, String>) = authority
        .call(
            "CheckAuthorization",
            &(subject, action, details, 1u32, String::new()),
        )
        .await?;
    Ok(authorized)
}
