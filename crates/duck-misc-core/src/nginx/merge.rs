//! 「预设 + JSON 覆盖」：把用户写的片段合到预设上（`custom` 模式的实现）。
//!
//! 参考仓的 `custom` 是**一整个表单**（几十个输入框逐项填）。本仓的执行页由
//! `[io]` 声明驱动，摆不下那种动态表单，所以改成：**选一个预设当底**，再用一段
//! JSON 覆盖想改的字段。合并规则极简：
//!
//! - 对象：逐键递归；
//! - 其它（数组、标量）：整体替换 —— 数组只换不拼，因为「合并两个 location 列表」
//!   没有说得通的语义。
//!
//! 结构体都带 `deny_unknown_fields`，所以键名写错会**当场报错**（`unknown field
//! 'listne_port'`），而不是被静默忽略。

use serde_json::Value;

use super::config::NginxConfig;

/// 把 `patch` 覆盖到 `base` 上，返回新配置。
pub fn apply_patch(base: &NginxConfig, patch: &Value) -> Result<NginxConfig, serde_json::Error> {
    let mut value = serde_json::to_value(base)?;
    merge(&mut value, patch);
    serde_json::from_value(value)
}

fn merge(target: &mut Value, patch: &Value) {
    match (target, patch) {
        (Value::Object(target_map), Value::Object(patch_map)) => {
            for (key, value) in patch_map {
                match target_map.get_mut(key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        target_map.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (target, patch) => *target = patch.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scalar_override_keeps_the_rest_of_the_preset() {
        let base = NginxConfig::default();
        let patched = apply_patch(&base, &json!({ "server": { "listen_port": "8180" } })).unwrap();
        assert_eq!(patched.server.listen_port, "8180");
        assert_eq!(patched.server.server_name, base.server.server_name);
        assert_eq!(patched.performance.gzip_comp_level, "5");
    }

    #[test]
    fn arrays_are_replaced_not_merged() {
        let base = NginxConfig::default();
        let patched = apply_patch(
            &base,
            &json!({
                "locations": [{
                    "path": "/api/",
                    "location_type": "proxy",
                    "target": "http://backend",
                    "proxy_buffer_size": "4k",
                    "proxy_buffers": "8 4k",
                    "proxy_read_timeout": "60",
                    "proxy_connect_timeout": "60",
                    "autoindex": false,
                    "extra_directives": ""
                }]
            }),
        )
        .unwrap();
        assert_eq!(patched.locations.len(), 1);
        assert_eq!(patched.locations[0].path, "/api/");
    }

    #[test]
    fn unknown_keys_are_rejected_instead_of_silently_dropped() {
        let error = apply_patch(
            &NginxConfig::default(),
            &json!({ "server": { "listne": "1" } }),
        )
        .expect_err("拼错的键必须报错");
        assert!(error.to_string().contains("listne"), "{error}");
    }

    #[test]
    fn partial_objects_keep_untouched_fields() {
        // 对象是**递归**合并的，所以只写想改的字段就够，不必抄一整块。
        let patched = apply_patch(
            &NginxConfig::default(),
            &json!({ "security": { "hsts": true } }),
        )
        .unwrap();
        assert!(patched.security.hsts);
        assert_eq!(patched.security.x_frame_options, "SAMEORIGIN");
    }
}
