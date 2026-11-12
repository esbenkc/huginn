//! Import maps (HTML, "Import maps"): `<script type="importmap">` maps module
//! specifiers such as `react` to URLs, for every module or only for modules
//! under a scope URL. Several import maps merge into one; a rule that
//! conflicts with an earlier one, or that would change how an already
//! resolved specifier resolves, is ignored. `integrity` is accepted and not
//! used.

use std::collections::HashMap;

use url::Url;

/// Parsed JSON, as far as import maps look into it.
#[derive(Debug)]
pub(crate) enum Json {
    Str(String),
    /// An object, in property order.
    Map(Vec<(String, Json)>),
    /// Anything else (arrays, numbers, null, booleans).
    Other,
}

/// Keys with their targets (`None`: blocked by an invalid entry), sorted in
/// descending code-unit order so longer prefixes come first.
type SpecifierMap = Vec<(String, Option<Url>)>;

#[derive(Debug, Default)]
pub(crate) struct ImportMap {
    imports: SpecifierMap,
    scopes: Vec<(String, SpecifierMap)>,
}

/// The page's import map and its resolved module set: the specifiers
/// resolved so far, which later import maps must not remap.
#[derive(Debug, Default)]
pub(crate) struct Resolver {
    map: ImportMap,
    /// (serialized base URL, normalized specifier) -> whether the specifier
    /// is not a URL or is a special URL.
    resolved: HashMap<(String, String), bool>,
}

fn sort_desc<T>(v: &mut [(String, T)]) {
    v.sort_by(|a, b| b.0.encode_utf16().cmp(a.0.encode_utf16()));
}

fn is_special(u: &Url) -> bool {
    matches!(u.scheme(), "http" | "https" | "ws" | "wss" | "ftp" | "file")
}

/// HTML "resolve a URL-like module specifier": absolute URLs and specifiers
/// starting with `/`, `./` or `../`.
fn url_like(spec: &str, base: &Url) -> Option<Url> {
    if spec.starts_with('/') || spec.starts_with("./") || spec.starts_with("../") {
        return base.join(spec).ok();
    }
    Url::parse(spec).ok()
}

/// HTML "sort and normalize a module specifier map".
fn normalize_map(map: &[(String, Json)], base: &Url) -> SpecifierMap {
    let mut out: SpecifierMap = Vec::new();
    for (key, value) in map {
        if key.is_empty() {
            continue;
        }
        let key_url = url_like(key, base);
        let key = key_url.map_or_else(|| key.clone(), String::from);
        let target = match value {
            Json::Str(s) => {
                url_like(s, base).filter(|u| !key.ends_with('/') || u.as_str().ends_with('/'))
            }
            _ => None,
        };
        match out.iter_mut().find(|e| e.0 == key) {
            Some(e) => e.1 = target,
            None => out.push((key, target)),
        }
    }
    sort_desc(&mut out);
    out
}

impl ImportMap {
    /// HTML "parse an import map string", after the JSON parse. `Err` is a
    /// TypeError message.
    pub(crate) fn parse(json: &Json, base: &Url) -> Result<ImportMap, String> {
        let Json::Map(top) = json else {
            return Err("An import map must be a JSON object.".into());
        };
        let mut map = ImportMap::default();
        for (key, value) in top {
            match (key.as_str(), value) {
                ("imports", Json::Map(m)) => map.imports = normalize_map(m, base),
                ("scopes", Json::Map(m)) => {
                    for (prefix, inner) in m {
                        let Json::Map(inner) = inner else {
                            return Err(format!(
                                "The value of the scope with prefix \"{prefix}\" must be a JSON object."
                            ));
                        };
                        let Ok(prefix) = base.join(prefix) else {
                            continue;
                        };
                        let prefix = String::from(prefix);
                        let inner = normalize_map(inner, base);
                        match map.scopes.iter_mut().find(|e| e.0 == prefix) {
                            Some(e) => e.1 = inner,
                            None => map.scopes.push((prefix, inner)),
                        }
                    }
                    sort_desc(&mut map.scopes);
                }
                ("integrity", Json::Map(_)) => {}
                ("imports" | "scopes" | "integrity", _) => {
                    return Err(format!(
                        "The \"{key}\" top-level value of an import map must be a JSON object."
                    ));
                }
                _ => {}
            }
        }
        Ok(map)
    }
}

/// HTML "resolve an imports match".
fn imports_match(
    normalized: &str,
    as_url: Option<&Url>,
    map: &SpecifierMap,
) -> Result<Option<Url>, String> {
    let blocked = |key: &str| format!("Resolution of \"{key}\" was blocked by a null entry.");
    for (key, target) in map {
        if key == normalized {
            return target.clone().map(Some).ok_or_else(|| blocked(key));
        }
        if key.ends_with('/')
            && normalized.starts_with(key.as_str())
            && as_url.is_none_or(is_special)
        {
            let Some(target) = target else {
                return Err(blocked(key));
            };
            let after = &normalized[key.len()..];
            let url = target
                .join(after)
                .ok()
                .filter(|u| u.as_str().starts_with(target.as_str()));
            return url.map(Some).ok_or_else(|| {
                format!("Resolution of \"{normalized}\" was blocked: it backtracks above its prefix \"{key}\".")
            });
        }
    }
    Ok(None)
}

/// Keep the entries of `new` whose keys `old` does not have.
fn merge_maps(old: &mut SpecifierMap, new: SpecifierMap) {
    for (k, v) in new {
        if !old.iter().any(|(o, _)| *o == k) {
            old.push((k, v));
        }
    }
    sort_desc(old);
}

impl Resolver {
    /// HTML "resolve a module specifier" for a module or script at `base`.
    pub(crate) fn resolve(&mut self, specifier: &str, base: &Url) -> Result<Url, String> {
        let as_url = url_like(specifier, base);
        let normalized = as_url.as_ref().map_or(specifier, Url::as_str);
        let base_str = base.as_str();
        let mut result = None;
        for (prefix, imports) in &self.map.scopes {
            if prefix == base_str
                || (prefix.ends_with('/') && base_str.starts_with(prefix.as_str()))
            {
                result = imports_match(normalized, as_url.as_ref(), imports)?;
                if result.is_some() {
                    break;
                }
            }
        }
        if result.is_none() {
            result = imports_match(normalized, as_url.as_ref(), &self.map.imports)?;
        }
        let Some(url) = result.or_else(|| as_url.clone()) else {
            return Err(format!(
                "Failed to resolve module specifier \"{specifier}\". Relative references must start with either \"/\", \"./\", or \"../\"."
            ));
        };
        self.resolved
            .entry((base_str.to_owned(), normalized.to_owned()))
            .or_insert_with(|| as_url.as_ref().is_none_or(is_special));
        Ok(url)
    }

    /// HTML "merge existing and new import maps".
    pub(crate) fn merge(&mut self, new: ImportMap) {
        let ImportMap {
            imports: mut new_imports,
            scopes: new_scopes,
        } = new;
        for (prefix, mut imports) in new_scopes {
            for ((base, spec), null_or_special) in &self.resolved {
                if *base == prefix || (prefix.ends_with('/') && base.starts_with(prefix.as_str())) {
                    imports.retain(|(key, _)| {
                        !(key == spec
                            || (key.ends_with('/')
                                && spec.starts_with(key.as_str())
                                && *null_or_special))
                    });
                }
            }
            match self.map.scopes.iter_mut().find(|e| e.0 == prefix) {
                Some(e) => merge_maps(&mut e.1, imports),
                None => self.map.scopes.push((prefix, imports)),
            }
        }
        sort_desc(&mut self.map.scopes);
        for (_, spec) in self.resolved.keys() {
            new_imports.retain(|(key, _)| !key.starts_with(spec.as_str()));
        }
        merge_maps(&mut self.map.imports, new_imports);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, Json)]) -> Json {
        Json::Map(
            entries
                .iter()
                .map(|(k, v)| (k.to_string(), clone(v)))
                .collect(),
        )
    }
    fn clone(j: &Json) -> Json {
        match j {
            Json::Str(s) => Json::Str(s.clone()),
            Json::Map(m) => Json::Map(m.iter().map(|(k, v)| (k.clone(), clone(v))).collect()),
            Json::Other => Json::Other,
        }
    }
    fn s(v: &str) -> Json {
        Json::Str(v.into())
    }
    fn base() -> Url {
        Url::parse("https://example.com/app/index.html").unwrap()
    }
    fn resolver(json: Json) -> Resolver {
        let mut r = Resolver::default();
        r.merge(ImportMap::parse(&json, &base()).unwrap());
        r
    }
    fn res(r: &mut Resolver, spec: &str, from: &str) -> Result<String, String> {
        r.resolve(spec, &Url::parse(from).unwrap())
            .map(String::from)
    }

    #[test]
    fn bare_specifiers_and_prefixes() {
        let mut r = resolver(map(&[(
            "imports",
            map(&[
                ("react", s("/vendor/react.js")),
                ("lodash/", s("https://cdn.example.net/lodash/")),
                ("lodash/fp/", s("./fp/")),
            ]),
        )]));
        let from = "https://example.com/app/main.js";
        assert_eq!(
            res(&mut r, "react", from).unwrap(),
            "https://example.com/vendor/react.js"
        );
        assert_eq!(
            res(&mut r, "lodash/map.js", from).unwrap(),
            "https://cdn.example.net/lodash/map.js"
        );
        // The longest prefix wins.
        assert_eq!(
            res(&mut r, "lodash/fp/map.js", from).unwrap(),
            "https://example.com/app/fp/map.js"
        );
        // Relative and absolute specifiers still resolve against the referrer.
        assert_eq!(
            res(&mut r, "./a.js", from).unwrap(),
            "https://example.com/app/a.js"
        );
        assert!(
            res(&mut r, "vue", from)
                .unwrap_err()
                .contains("Failed to resolve module specifier \"vue\"")
        );
    }

    #[test]
    fn urls_can_be_remapped_and_scopes_win() {
        let mut r = resolver(map(&[
            (
                "imports",
                map(&[
                    ("/app/helper.js", s("/app/helper-v2.js")),
                    ("dep", s("/dep-1.js")),
                ]),
            ),
            (
                "scopes",
                map(&[("/app/legacy/", map(&[("dep", s("/dep-0.js"))]))]),
            ),
        ]));
        assert_eq!(
            res(&mut r, "./helper.js", "https://example.com/app/main.js").unwrap(),
            "https://example.com/app/helper-v2.js"
        );
        assert_eq!(
            res(&mut r, "dep", "https://example.com/app/main.js").unwrap(),
            "https://example.com/dep-1.js"
        );
        assert_eq!(
            res(&mut r, "dep", "https://example.com/app/legacy/x.js").unwrap(),
            "https://example.com/dep-0.js"
        );
    }

    #[test]
    fn invalid_entries_block_and_backtracking_fails() {
        let mut r = resolver(map(&[(
            "imports",
            map(&[
                ("bad", Json::Other),
                ("pkg/", s("/pkg")),
                ("up/", s("/up/")),
            ]),
        )]));
        let from = "https://example.com/app/main.js";
        assert!(
            res(&mut r, "bad", from)
                .unwrap_err()
                .contains("blocked by a null entry")
        );
        // A prefix key must map to a URL ending in "/".
        assert!(
            res(&mut r, "pkg/a.js", from)
                .unwrap_err()
                .contains("blocked by a null entry")
        );
        assert!(
            res(&mut r, "up/../../secret.js", from)
                .unwrap_err()
                .contains("backtracks")
        );
    }

    #[test]
    fn parse_errors() {
        assert!(ImportMap::parse(&Json::Other, &base()).is_err());
        assert!(ImportMap::parse(&map(&[("imports", s("x"))]), &base()).is_err());
        assert!(ImportMap::parse(&map(&[("scopes", map(&[("/a/", s("x"))]))]), &base()).is_err());
        assert!(ImportMap::parse(&map(&[("other", s("x"))]), &base()).is_ok());
    }

    #[test]
    fn merging_keeps_earlier_rules_and_resolved_specifiers() {
        let mut r = resolver(map(&[("imports", map(&[("a", s("/a-1.js"))]))]));
        let from = "https://example.com/app/main.js";
        assert_eq!(
            res(&mut r, "a", from).unwrap(),
            "https://example.com/a-1.js"
        );
        // "c" was not mapped yet but has been resolved (and failed): resolving
        // records only successes, so a later map may still define it.
        assert!(res(&mut r, "c", from).is_err());
        let second = map(&[(
            "imports",
            map(&[("a", s("/a-2.js")), ("b", s("/b.js")), ("c", s("/c.js"))]),
        )]);
        r.merge(ImportMap::parse(&second, &base()).unwrap());
        assert_eq!(
            res(&mut r, "a", from).unwrap(),
            "https://example.com/a-1.js"
        );
        assert_eq!(res(&mut r, "b", from).unwrap(), "https://example.com/b.js");
        assert_eq!(res(&mut r, "c", from).unwrap(), "https://example.com/c.js");
        // "b" is resolved now, so a third map cannot change it.
        r.merge(
            ImportMap::parse(
                &map(&[("scopes", map(&[("/app/", map(&[("b", s("/b-2.js"))]))]))]),
                &base(),
            )
            .unwrap(),
        );
        assert_eq!(res(&mut r, "b", from).unwrap(), "https://example.com/b.js");
    }
}
