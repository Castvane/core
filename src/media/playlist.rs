use url::Url;

pub fn rewrite(
    text: &str,
    path: &str,
    authority: Option<&str>,
    session: &str,
    token: Option<&str>,
) -> String {
    let base = Url::parse(&format!(
        "http://{}{}",
        authority.unwrap_or("localhost"),
        path
    ))
    .ok();
    let mut result = String::with_capacity(text.len() + 256);
    for line in text.lines() {
        if line.starts_with('#') {
            let mut rest = line;
            while let Some((prefix, tail)) = rest.split_once("URI=\"") {
                let Some((uri, right)) = tail.split_once('"') else {
                    break;
                };
                result.push_str(prefix);
                result.push_str("URI=\"");
                result.push_str(&rewrite_url(uri, base.as_ref(), session, token));
                result.push('"');
                rest = right;
            }
            result.push_str(rest);
        } else if !line.trim().is_empty() {
            result.push_str(&rewrite_url(line.trim(), base.as_ref(), session, token));
        }
        result.push('\n');
    }
    result
}

fn rewrite_url(uri: &str, base: Option<&Url>, session: &str, token: Option<&str>) -> String {
    let Some(base) = base else {
        return uri.to_owned();
    };
    let Ok(mut url) = base.join(uri) else {
        return uri.to_owned();
    };
    if url.host_str() != base.host_str()
        || url.port() != base.port()
        || !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return uri.to_owned();
    }
    let mut values: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| key != "session" && (token.is_none() || key != "token"))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    values.push(("session".into(), session.into()));
    if let Some(token) = token {
        values.push(("token".into(), token.into()));
    }
    url.query_pairs_mut().clear().extend_pairs(values);
    let (prefix, fragment) = uri
        .split_once('#')
        .map_or((uri, None), |(prefix, fragment)| (prefix, Some(fragment)));
    let path = if token.is_some() && (prefix.starts_with("//") || prefix.contains("://")) {
        url.path()
    } else {
        prefix.split_once('?').map_or(prefix, |(path, _)| path)
    };
    format!(
        "{path}?{}{}",
        url.query().unwrap_or_default(),
        fragment.map_or_else(String::new, |f| format!("#{f}"))
    )
}
