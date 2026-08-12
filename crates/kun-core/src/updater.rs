//! 自动更新检查：查询 GitHub Releases 最新版本并比较。
//!
//! 轻量实现（ureq 同步 HTTP），在后台线程调用，不阻塞 UI。

/// 更新信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    /// 最新版本号（如 "0.2.0"）。
    pub version: String,
    /// 下载页面（GitHub Release）。
    pub url: String,
    /// 发布说明摘要。
    pub notes: String,
}

/// 默认仓库（可被测试覆盖）。
pub const DEFAULT_REPO: &str = "yqstart/kun";

/// 检查是否有新版本。
///
/// 返回 `Ok(None)` 表示已是最新；`Ok(Some(info))` 表示有新版；
/// `Err(msg)` 表示检查失败（网络/API 错误等，UI 可静默处理）。
pub fn check_for_update(current_version: &str, repo: &str) -> Result<Option<UpdateInfo>, String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .build()
        .new_agent();
    let mut response = agent
        .get(&url)
        .header("User-Agent", format!("kun/{current_version}"))
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("请求失败：{e}"))?;

    let status = response.status();
    if status == 404 {
        // 仓库无任何 Release。
        return Ok(None);
    }
    if status == 403 || status == 429 {
        // 未认证 API 限流：静默视为无更新，避免打扰用户。
        return Ok(None);
    }
    if !status.is_success() {
        return Err(format!("GitHub API 返回 {status}"));
    }

    // 解析 JSON（手写轻量提取，避免引入 serde_json 依赖）。
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("读取响应失败：{e}"))?;
    let latest = parse_latest_release(&body)?;
    let latest_version = latest.tag_name.trim_start_matches('v').to_string();

    if version_newer(&latest_version, current_version) {
        Ok(Some(UpdateInfo {
            version: latest_version,
            url: latest.html_url,
            notes: latest.body,
        }))
    } else {
        Ok(None)
    }
}

/// 解析 GitHub latest release API 响应中的 tag_name / html_url / body。
fn parse_latest_release(body: &str) -> Result<ReleaseFields, String> {
    let tag_name =
        extract_json_string(body, "tag_name").ok_or_else(|| "响应缺少 tag_name".to_string())?;
    let html_url =
        extract_json_string(body, "html_url").ok_or_else(|| "响应缺少 html_url".to_string())?;
    let notes = extract_json_string(body, "body").unwrap_or_default();
    Ok(ReleaseFields {
        tag_name,
        html_url,
        body: notes,
    })
}

struct ReleaseFields {
    tag_name: String,
    html_url: String,
    body: String,
}

/// 从 JSON 对象中提取字符串字段值（`"key": "value"`，处理转义）。
fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{key}\"\\s*:");
    let re = regex::Regex::new(&pattern).ok()?;
    let m = re.find(json)?;
    let rest = &json[m.end()..];
    let rest = rest.trim_start();
    if !rest.starts_with('"') {
        return None;
    }
    let mut out = String::new();
    let mut chars = rest[1..].chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => {
                if let Some(next) = chars.next() {
                    out.push(match next {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        'u' => {
                            // 简单处理：跳过 unicode 转义（保持原文近似）。
                            let _ = chars.by_ref().take(4).count();
                            '?'
                        }
                        other => other,
                    });
                }
            }
            other => out.push(other),
        }
    }
    None
}

/// 语义化版本比较：`newer > current` 时返回 true。
///
/// 仅比较数字段（主.次.补丁），忽略预发布后缀。
pub fn version_newer(newer: &str, current: &str) -> bool {
    let a = parse_version(newer);
    let b = parse_version(current);
    for i in 0..3 {
        match a[i].cmp(&b[i]) {
            std::cmp::Ordering::Greater => return true,
            std::cmp::Ordering::Less => return false,
            std::cmp::Ordering::Equal => continue,
        }
    }
    false
}

/// 解析 "x.y.z" 为数字三元组（缺失段补 0）。
fn parse_version(v: &str) -> [u32; 3] {
    let mut parts = v
        .trim()
        .trim_start_matches('v')
        .split('.')
        .map(|s| {
            s.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
        })
        .map(|s| s.parse::<u32>().unwrap_or(0));
    [
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 版本比较() {
        assert!(version_newer("0.2.0", "0.1.0"));
        assert!(version_newer("1.0.0", "0.9.9"));
        assert!(version_newer("0.1.10", "0.1.9"));
        assert!(!version_newer("0.1.0", "0.1.0"));
        assert!(!version_newer("0.1.0", "0.2.0"));
        assert!(!version_newer("0.1.0-beta", "0.1.0"));
        // 前缀 v 容忍。
        assert!(version_newer("v0.3.0", "0.2.0"));
    }

    #[test]
    fn 解析release响应() {
        let json = "{\n  \"tag_name\": \"v0.2.0\",\n  \"html_url\": \"https://github.com/yqstart/kun/releases/tag/v0.2.0\",\n  \"body\": \"## 更新\\n- 新增功能\\n- 修复问题\"\n}";
        let fields = parse_latest_release(json).unwrap();
        assert_eq!(fields.tag_name, "v0.2.0");
        assert_eq!(
            fields.html_url,
            "https://github.com/yqstart/kun/releases/tag/v0.2.0"
        );
        assert!(fields.body.contains("新增功能"));
    }

    #[test]
    fn 版本段缺失补齐() {
        assert_eq!(parse_version("1"), [1, 0, 0]);
        assert_eq!(parse_version("1.2"), [1, 2, 0]);
        assert_eq!(parse_version("1.2.3.4"), [1, 2, 3]);
        assert_eq!(parse_version("abc"), [0, 0, 0]);
    }
}
