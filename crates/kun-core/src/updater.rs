//! 自动更新：查询 GitHub Releases 最新版本、下载资产。
//!
//! 轻量实现（ureq 同步 HTTP），在后台线程调用，不阻塞 UI。
//! 版本检查走 releases.atom（不受 API 限流影响），资产下载走
//! releases/download 直链（带进度回调，供 UI 显示进度条）。

/// 更新信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    /// 最新版本号（如 "0.2.0"）。
    pub version: String,
    /// 发布页面（GitHub Release）。
    pub url: String,
    /// 发布说明摘要。
    pub notes: String,
    /// 当前平台的 dmg 直链下载地址。
    pub asset_url: String,
    /// 资产文件名（如 `kun-0.2.0-macos-arm64.dmg`）。
    pub asset_name: String,
}

/// 默认仓库（可被测试覆盖）。
pub const DEFAULT_REPO: &str = "yqstart/kun";

/// 检查是否有新版本。
///
/// 返回 `Ok(None)` 表示已是最新；`Ok(Some(info))` 表示有新版；
/// `Err(msg)` 表示检查失败（网络/API 错误等，UI 可静默处理）。
pub fn check_for_update(
    current_version: &str,
    repo: &str,
    arch: &str,
) -> Result<Option<UpdateInfo>, String> {
    // 走 releases.atom（HTML feed）而非 REST API：API 未认证限流（60 次/小时/IP）
    // 极容易被共享出口 IP 耗尽导致检查永远失败；feed 不受该限流影响。
    let url = format!("https://github.com/{repo}/releases.atom");
    let agent = make_agent();
    let mut response = agent
        .get(&url)
        .header("User-Agent", format!("kun/{current_version}"))
        .header("Accept", "application/atom+xml")
        .call()
        .map_err(|e| format!("请求失败：{e}"))?;

    let status = response.status();
    if status == 404 {
        // 仓库不存在或无任何 Release。
        return Ok(None);
    }
    if !status.is_success() {
        return Err(format!("GitHub 返回 {status}"));
    }

    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("读取响应失败：{e}"))?;
    // 首个 <entry> 即最新发布（feed 按发布时间倒序）；无 entry 视为无 Release。
    let Some(latest) = parse_latest_entry(&body) else {
        return Ok(None);
    };
    let latest_version = latest.tag_name.trim_start_matches('v').to_string();

    if version_newer(&latest_version, current_version) {
        let asset_name = asset_name_for(&latest_version, arch);
        let asset_url = asset_url_for(repo, &latest.tag_name, &latest_version, arch);
        Ok(Some(UpdateInfo {
            version: latest_version,
            url: latest.html_url,
            notes: latest.body,
            asset_url,
            asset_name,
        }))
    } else {
        Ok(None)
    }
}

/// 统一的 ureq Agent 配置。
fn make_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        // 手动处理 4xx/5xx，否则 ureq 默认把状态码转成 Err，404 等分支不可达。
        .http_status_as_error(false)
        .build()
        .new_agent()
}

/// 当前平台资产的 dmg 文件名。
fn asset_name_for(version: &str, arch: &str) -> String {
    format!("kun-{version}-macos-{arch}.dmg")
}

/// 当前平台资产的下载直链。
fn asset_url_for(repo: &str, tag: &str, version: &str, arch: &str) -> String {
    format!(
        "https://github.com/{}/releases/download/{}/{}",
        repo.trim_end_matches('/'),
        tag,
        asset_name_for(version, arch)
    )
}

/// 下载资产到本地文件，并持续回调 `(已下载字节, 总字节)`。
///
/// GitHub 的下载直链会 302 到 S3，ureq 默认跟随重定向。
pub fn download_asset(
    url: &str,
    dest: &std::path::Path,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<(), String> {
    use std::io::{Read, Write};

    let agent = make_agent();
    let mut response = agent
        .get(url)
        .header("User-Agent", "kun-updater")
        .call()
        .map_err(|e| format!("下载请求失败：{e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("下载失败：HTTP {status}"));
    }

    let total = response
        .headers()
        .get("Content-Length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败：{e}"))?;
    }
    let mut file = std::fs::File::create(dest).map_err(|e| format!("创建文件失败：{e}"))?;
    let mut reader = response.body_mut().as_reader();
    let mut buf = [0u8; 256 * 1024];
    let mut downloaded: u64 = 0;
    on_progress(0, total);
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("下载中断：{e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("写入文件失败：{e}"))?;
        downloaded += n as u64;
        on_progress(downloaded, total);
    }
    file.sync_all().map_err(|e| format!("落盘失败：{e}"))?;
    Ok(())
}

/// 解析 releases.atom 首个 `<entry>`（GitHub 按发布时间倒序，首个即最新）。
///
/// 返回 `None` 表示 feed 中没有任何发布。
fn parse_latest_entry(body: &str) -> Option<ReleaseFields> {
    let entry_start = body.find("<entry")?;
    let entry_end = entry_start + body[entry_start..].find("</entry>")?;
    let entry = &body[entry_start..entry_end];

    // 发布页链接：<link rel="alternate" type="text/html" href=".../releases/tag/vX.Y.Z"/>
    let url = extract_attr(entry, "href")?;
    // 版本 tag 即 href 最后一个路径段（形如 v0.1.1）。
    let tag_name = url.rsplit('/').next()?.to_string();
    // 发布说明：<content ...>HTML</content>，转纯文本便于弹窗预览。
    let notes = extract_text(entry, "content").unwrap_or_default();
    let notes = html_to_text(&notes);

    Some(ReleaseFields {
        tag_name,
        html_url: url,
        body: notes,
    })
}

struct ReleaseFields {
    tag_name: String,
    html_url: String,
    body: String,
}

/// 提取 `<link ...>` 元素中指定属性的值（如 `href="..."`）。
fn extract_attr(s: &str, name: &str) -> Option<String> {
    let link = s.find("<link")?;
    let link_end = link + s[link..].find('>')?;
    let link_tag = &s[link..=link_end];
    let key = format!("{name}=\"");
    let start = link_tag.find(&key)? + key.len();
    let rest = &link_tag[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// 提取 `<tag ...>内容</tag>` 的文本内容。
fn extract_text(s: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = s.find(&open)?;
    let content_start = start + s[start..].find('>')? + 1;
    let close = format!("</{tag}>");
    let end = content_start + s[content_start..].find(&close)?;
    Some(s[content_start..end].to_string())
}

/// 将 HTML 片段转纯文本：还原实体、剥离标签，并保留段落/列表结构。
///
/// 块级标签边界（p/标题/li 等）产生换行，`<li>` 前补 `• ` 项目符号；
/// 行内标签（a/strong/code 等）仅以空格分隔，避免相邻内容粘连。
fn html_to_text(html: &str) -> String {
    let unescaped = unescape_html_entities(html);
    let mut out = String::new();
    let mut chars = unescaped.chars();
    while let Some(c) = chars.next() {
        if c != '<' {
            out.push(c);
            continue;
        }
        // 收集完整标签名（不含 < >）。
        let mut tag = String::new();
        for c2 in chars.by_ref() {
            if c2 == '>' {
                break;
            }
            tag.push(c2);
        }
        let name = tag
            .trim_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or("");
        let is_close = tag.starts_with('/');
        match name {
            // 列表项：换行 + 项目符号（仅开始标签）。
            "li" => {
                if !is_close {
                    out.push('\n');
                    out.push_str("• ");
                }
            }
            // 块级段落/标题/列表容器结束 → 换行。
            "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "ul" | "ol" | "blockquote" | "pre"
            | "table" | "tr" | "div" => {
                if is_close {
                    out.push('\n');
                }
            }
            // 自闭合换行标签。
            "br" | "hr" => out.push('\n'),
            // 行内标签 → 空格分隔。
            _ => out.push(' '),
        }
    }
    // 归一化：行内空白压缩、连续空行合并为单个段落间距、去首尾空行。
    let mut result = String::new();
    let mut blank = false;
    for line in out.lines() {
        let text = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            if !result.is_empty() && !blank {
                result.push('\n');
                blank = true;
            }
        } else {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&text);
            blank = false;
        }
    }
    result.trim_end().to_string()
}

/// 还原常见 HTML 实体（GitHub atom 的 `<content>` 为 HTML 转义文本）。
fn unescape_html_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
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
    fn 解析atom首个entry() {
        let feed = "<?xml version=\"1.0\"?>\n\
            <feed xmlns=\"http://www.w3.org/2005/Atom\">\n\
              <title>Release notes from kun</title>\n\
              <entry>\n\
                <link rel=\"alternate\" type=\"text/html\" href=\"https://github.com/yqstart/kun/releases/tag/v0.2.0\"/>\n\
                <title>kun v0.2.0</title>\n\
                <content type=\"html\">&lt;h2&gt;[0.2.0]&lt;/h2&gt;&lt;ul&gt;&lt;li&gt;新增功能&lt;/li&gt;&lt;/ul&gt;</content>\n\
              </entry>\n\
            </feed>";
        let fields = parse_latest_entry(feed).unwrap();
        assert_eq!(fields.tag_name, "v0.2.0");
        assert_eq!(
            fields.html_url,
            "https://github.com/yqstart/kun/releases/tag/v0.2.0"
        );
        assert!(fields.body.contains("新增功能"));
        // 标签与实体应被清理干净。
        assert!(!fields.body.contains('<') && !fields.body.contains('&'));
    }

    #[test]
    fn html转文本保留段落与列表结构() {
        let html =
            "<h2>[0.3.0]</h2><p>本次更新</p><ul><li>SFTP 连接状态</li><li>输入法延迟</li></ul>";
        let text = html_to_text(html);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            vec!["[0.3.0]", "本次更新", "", "• SFTP 连接状态", "• 输入法延迟"]
        );
        // 行内标签只产生空格分隔，不产生换行。
        let inline = html_to_text("<p>支持 <code>ssh</code> 与 <strong>sftp</strong></p>");
        assert_eq!(inline, "支持 ssh 与 sftp");
        // 自闭合换行标签。
        let br = html_to_text("第一行<br/>第二行");
        assert_eq!(br.lines().count(), 2);
    }

    #[test]
    fn 无entry返回none() {
        let feed = "<feed xmlns=\"http://www.w3.org/2005/Atom\"><title>empty</title></feed>";
        assert!(parse_latest_entry(feed).is_none());
    }

    #[test]
    fn 资产地址按架构构造() {
        assert_eq!(
            asset_name_for("0.2.0", "arm64"),
            "kun-0.2.0-macos-arm64.dmg"
        );
        assert_eq!(
            asset_url_for("yqstart/kun", "v0.2.0", "0.2.0", "x64"),
            "https://github.com/yqstart/kun/releases/download/v0.2.0/kun-0.2.0-macos-x64.dmg"
        );
    }

    #[test]
    fn 版本段缺失补齐() {
        assert_eq!(parse_version("1"), [1, 0, 0]);
        assert_eq!(parse_version("1.2"), [1, 2, 0]);
        assert_eq!(parse_version("1.2.3.4"), [1, 2, 3]);
        assert_eq!(parse_version("abc"), [0, 0, 0]);
    }
}
