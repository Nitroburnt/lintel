// What a copied text is and what may be done with it: a web link and its
// tracking parameters. Text in, answer out; nothing here touches the clipboard.

use percent_encoding::percent_decode_str;

/// Tracking parameters removed on every site; `utm_` is matched as a prefix.
const EVERYWHERE: &[&str] = &[
    "fbclid", "gclid", "dclid", "gbraid", "wbraid", "msclkid", "yclid", "mc_cid", "mc_eid", "igshid", "twclid",
    "ttclid", "li_fat_id", "mkt_tok", "_hsenc", "_hsmi", "__twitter_impression", "fb_action_ids", "fb_action_types",
    "fb_source", "mibextid",
];

/// Trackers only one site uses, for that host and its subdomains: `si` is a
/// share token on YouTube and a real parameter elsewhere, `t` a tracker on X
/// and the playback position on YouTube.
const SITES: &[(&str, &[&str])] = &[
    ("youtube.com", &["si", "pp", "feature", "kw"]),
    ("youtu.be", &["si", "pp", "feature", "kw"]),
    ("twitter.com", &["s", "t", "cn", "src", "refsrc", "ref_src", "ref_url"]),
    ("x.com", &["s", "t", "cn", "src", "refsrc", "ref_src", "ref_url"]),
    ("instagram.com", &["igsh"]),
    ("spotify.com", &["si"]),
    (
        "reddit.com",
        &["correlation_id", "ref_campaign", "ref_source", "rdt", "share_id", "_branch_match_id", "$deep_link", "$3p", "$original_url"],
    ),
    ("tiktok.com", &["u_code", "preview_pb", "_d", "_t", "_r", "timestamp", "user_id", "share_app_name", "share_iid"]),
    (
        "bilibili.com",
        &[
            "spm_id_from", "from_spmid", "from_source", "share_source", "share_from", "share_medium", "share_plat",
            "share_tag", "share_session_id", "msource", "refer_from", "seid", "unique_k", "vd_source", "plat_id",
            "buvid", "bbid", "up_id", "is_story_h5", "timestamp", "ts", "visit_id", "session_id", "broadcast_type",
            "is_room_feed",
        ],
    ),
    (
        "xiaohongshu.com",
        &[
            "xhsshare", "author_share", "xsec_source", "share_from_user_hidden", "shareredid", "share_id", "exsource",
            "app_version", "app_platform", "apptime", "appuid",
        ],
    ),
];

/// The host of a text that is one http(s) link and nothing else.
fn web_host(text: &str) -> Option<&str> {
    if text.contains(char::is_whitespace) {
        return None;
    }
    let (scheme, rest) = text.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    // Without the port; an IPv6 address keeps its brackets.
    let host = if host.starts_with('[') { host.split_inclusive(']').next()? } else { host.split(':').next()? };
    (!host.is_empty()).then_some(host)
}

/// One web address and nothing else.
pub fn is_web(text: &str) -> bool {
    web_host(text.trim()).is_some()
}

/// A link without its tracking parameters, and the names that were taken out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cleaned {
    pub url: String,
    pub removed: Vec<String>,
}

fn tracked(host: &str, name: &str, extra: &[String]) -> bool {
    name.starts_with("utm_")
        || EVERYWHERE.contains(&name)
        || extra.iter().any(|e| e == name)
        || SITES
            .iter()
            .any(|(site, names)| (host == *site || host.ends_with(&format!(".{site}"))) && names.contains(&name))
}

/// Removes the tracking parameters of a link. `None` when `text` is not one
/// web link; `removed` is empty when there was nothing to take out. The rest of
/// the link is kept exactly as written: nothing is re-encoded or re-ordered,
/// the `?` goes with the last parameter and the `#fragment` stays.
pub fn clean(text: &str, extra: &[String]) -> Option<Cleaned> {
    let link = text.trim();
    let host = web_host(link)?.to_lowercase();
    let fragment = link.find('#').unwrap_or(link.len());
    let Some(question) = link[..fragment].find('?') else {
        return Some(Cleaned { url: link.to_string(), removed: Vec::new() });
    };
    let mut removed: Vec<String> = Vec::new();
    let kept: Vec<&str> = link[question + 1..fragment]
        .split('&')
        .filter(|pair| {
            // The name half, decoded: `%75tm_source` is still utm_source. A pair without `=` is a bare flag.
            let raw = pair.split('=').next().unwrap_or_default();
            let name = percent_decode_str(raw).decode_utf8().map_or_else(|_| raw.to_string(), |n| n.into_owned());
            if !tracked(&host, &name.to_lowercase(), extra) {
                return true;
            }
            if !removed.contains(&name) {
                removed.push(name);
            }
            false
        })
        .collect();
    if removed.is_empty() {
        return Some(Cleaned { url: link.to_string(), removed });
    }
    let query = if kept.is_empty() { String::new() } else { format!("?{}", kept.join("&")) };
    Some(Cleaned { url: format!("{}{query}{}", &link[..question], &link[fragment..]), removed })
}

/// The checks the probe runs before it touches the clipboard.
pub fn self_check() {
    let none: [String; 0] = [];
    let c = clean("https://example.com/a?utm_source=x&id=7&fbclid=abc#top", &none).unwrap();
    assert_eq!((c.url.as_str(), c.removed.len()), ("https://example.com/a?id=7#top", 2));
    assert_eq!(clean("https://example.com/a?%75tm_medium=x", &none).unwrap().url, "https://example.com/a");
    // `si` goes on YouTube only, `t` stays there and goes on X.
    assert_eq!(clean("https://www.youtube.com/watch?v=1&t=42&si=abc", &none).unwrap().url, "https://www.youtube.com/watch?v=1&t=42");
    assert_eq!(clean("https://x.com/a/status/1?t=abc&s=20", &none).unwrap().url, "https://x.com/a/status/1");
    assert_eq!(clean("https://example.com/?si=abc", &none).unwrap().removed.len(), 0);
    assert_eq!(clean("https://example.com/?ref=a&b=1", &["ref".to_string()]).unwrap().url, "https://example.com/?b=1");
    assert!(clean("see https://example.com/?utm_source=x", &none).is_none());
    assert!(clean("ftp://example.com/?utm_source=x", &none).is_none());
}
