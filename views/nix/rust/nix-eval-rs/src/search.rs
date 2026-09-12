//! Package metadata is evaluated once; filtering and presentation reuse that catalogue.
use regex::{Regex, RegexBuilder};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;

type Result<T> = std::result::Result<T, String>;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Package {
    pub path: String,
    pub pname: String,
    pub version: String,
    pub description: String,
}

#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct Catalogue(pub BTreeMap<String, Package>);

impl Catalogue {
    pub fn insert(&mut self, path: String, name: &str, description: String) -> Result<()> {
        let split = crate::drv_name::split(name.as_bytes());
        let package = Package {
            path: path.clone(),
            pname: std::str::from_utf8(split.name)
                .map_err(|e| e.to_string())?
                .to_owned(),
            version: std::str::from_utf8(split.version)
                .map_err(|e| e.to_string())?
                .to_owned(),
            description,
        };
        if self.0.insert(path.clone(), package).is_some() {
            return Err(format!("duplicate search package path '{path}'"));
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<String> {
        let rows: Vec<_> = self
            .0
            .values()
            .map(|p| json!([p.path, p.pname, p.version, p.description]))
            .collect();
        serde_json::to_string(&json!({"version":1,"packages":rows})).map_err(|e| e.to_string())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let value: Json = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        let object = value
            .as_object()
            .ok_or("search catalogue must be an object")?;
        if object.len() != 2 || object.get("version").and_then(Json::as_u64) != Some(1) {
            return Err("unknown search catalogue version or fields".into());
        }
        let rows = object
            .get("packages")
            .and_then(Json::as_array)
            .ok_or("search packages must be an array")?;
        let mut found = BTreeMap::new();
        for row in rows {
            let row = row.as_array().ok_or("invalid search package row")?;
            let [path, pname, version, description] = row.as_slice() else {
                return Err("invalid search package fields".into());
            };
            let text = |value: &Json| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "search package field must be a string".to_owned())
            };
            let package = Package {
                path: text(path)?,
                pname: text(pname)?,
                version: text(version)?,
                description: text(description)?,
            };
            if found
                .last_key_value()
                .is_some_and(|(last, _)| last >= &package.path)
            {
                return Err("search package paths must be unique and sorted".into());
            }
            found.insert(package.path.clone(), package);
        }
        Ok(Self(found))
    }
}

pub(crate) struct Plan {
    include: Vec<Regex>,
    exclude: Vec<Regex>,
    json: bool,
    colored: bool,
}

impl Plan {
    pub fn new(include: &[String], exclude: &[String], json: bool, colored: bool) -> Result<Self> {
        if include.is_empty() {
            return Err("search requires at least one regular expression".into());
        }
        let compile = |patterns: &[String]| {
            patterns
                .iter()
                .map(|pattern| {
                    RegexBuilder::new(pattern)
                        .case_insensitive(true)
                        .build()
                        .map_err(|e| format!("invalid search regular expression '{pattern}': {e}"))
                })
                .collect::<Result<Vec<_>>>()
        };
        Ok(Self {
            include: compile(include)?,
            exclude: compile(exclude)?,
            json,
            colored,
        })
    }

    fn matches(&self, package: &Package) -> bool {
        let matches = |regex: &Regex| {
            [&package.path, &package.pname, &package.description]
                .into_iter()
                .any(|text| regex.is_match(text))
        };
        self.include.iter().all(matches) && !self.exclude.iter().any(matches)
    }

    fn highlight(&self, text: &str, bold: bool) -> Result<String> {
        let sanitized = crate::terminal::terminal_text(text);
        let text = sanitized.as_ref();
        if !self.colored {
            return Ok(text.to_owned());
        }
        let mut ranges: Vec<_> = self
            .include
            .iter()
            .flat_map(|regex| regex.find_iter(text))
            .filter(|matched| !matched.is_empty())
            .map(|matched| matched.range())
            .collect();
        ranges.sort_unstable_by_key(|range| (range.start, range.end));
        let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
        for range in ranges {
            if let Some(last) = merged.last_mut()
                && range.start <= last.end
            {
                last.end = last.end.max(range.end);
            } else {
                merged.push(range);
            }
        }
        let mut out = String::new();
        let mut offset = 0;
        for range in merged {
            out.push_str(
                text.get(offset..range.start)
                    .ok_or("invalid search highlight boundary")?,
            );
            out.push_str("\x1b[32;1m");
            out.push_str(
                text.get(range.clone())
                    .ok_or("invalid search highlight range")?,
            );
            out.push_str(if bold { "\x1b[0;1m" } else { "\x1b[0m" });
            offset = range.end;
        }
        out.push_str(text.get(offset..).ok_or("invalid search highlight tail")?);
        Ok(out)
    }

    pub fn render(&self, catalogue: &Catalogue) -> Result<String> {
        if self.json {
            let packages: serde_json::Map<_, _> = catalogue
                .0
                .values()
                .filter(|p| self.matches(p))
                .map(|p| {
                    (
                        p.path.clone(),
                        json!({"pname":p.pname,"version":p.version,"description":p.description}),
                    )
                })
                .collect();
            return serde_json::to_string(&packages)
                .map(|text| text + "\n")
                .map_err(|e| e.to_string());
        }
        let mut output = String::new();
        for package in catalogue.0.values().filter(|p| self.matches(p)) {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str("* ");
            if self.colored {
                output.push_str("\x1b[0;1m");
            }
            output.push_str(&self.highlight(&package.path, true)?);
            if self.colored {
                output.push_str("\x1b[0m");
            }
            output.push_str(&format!(
                " ({})\n",
                crate::terminal::terminal_text(&package.version)
            ));
            if !package.description.is_empty() {
                output.push_str("  ");
                output.push_str(&self.highlight(&package.description, false)?);
                output.push('\n');
            }
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn catalogue() -> Result<Catalogue> {
        let mut result = Catalogue::default();
        result.insert("hello".into(), "hello-0.1", "Empty\n file".into())?;
        result.insert("foo".into(), "foo-5", String::new())?;
        result.insert("bar".into(), "bar-3", "broken bar".into())?;
        Ok(result)
    }
    #[test]
    fn codec_and_filter_policy() -> Result<()> {
        let catalogue = catalogue()?;
        assert_eq!(
            Catalogue::decode(catalogue.encode()?.as_bytes())?,
            catalogue
        );
        let render = |include: &[&str], exclude: &[&str]| {
            Plan::new(
                &include.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
                &exclude.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
                true,
                false,
            )?
            .render(&catalogue)
        };
        assert!(render(&["HELLO", "empty"], &[])?.contains("hello"));
        assert_eq!(render(&["hello", "broken"], &[])?, "{}\n");
        assert!(!render(&["^"], &["bar"])?.contains("bar"));
        assert!(Plan::new(&[], &[], false, false).is_err());
        assert!(Plan::new(&["[".into()], &[], false, false).is_err());
        Ok(())
    }
    #[test]
    fn overlapping_and_zero_width_matches() -> Result<()> {
        let plan = Plan::new(
            &["broken b".into(), "en bar".into(), "^".into()],
            &[],
            false,
            true,
        )?;
        assert_eq!(
            plan.highlight("broken bar", false)?,
            "\x1b[32;1mbroken bar\x1b[0m"
        );
        let plain = Plan::new(&["^".into()], &[], false, true)?;
        assert_eq!(plain.highlight("éclair", false)?, "éclair");
        Ok(())
    }
    #[test]
    fn invalid_cache_rows_are_rejected() {
        for bytes in [
            r#"{}"#,
            r#"{"version":2,"packages":[]}"#,
            r#"{"version":1,"packages":[["x","p",0,""]]}"#,
            r#"{"version":1,"packages":[["x","p","1",""],["x","p","1",""]]}"#,
        ] {
            assert!(Catalogue::decode(bytes.as_bytes()).is_err());
        }
    }

    #[test]
    fn terminal_controls_are_visible_while_json_preserves_metadata() -> Result<()> {
        let mut catalogue = Catalogue::default();
        let description = "safe\x1b]52;c;payload\x07\n\u{009b}31méclair";
        catalogue.insert("package".into(), "package-1", description.into())?;
        let text = Plan::new(&["^".into()], &[], false, false)?.render(&catalogue)?;
        assert!(!text.contains('\x1b'));
        assert!(!text.contains('\x07'));
        assert!(!text.contains('\u{009b}'));
        assert!(text.contains("\\u{1b}]52;c;payload\\u{7}\\n\\u{9b}31méclair"));
        let json = Plan::new(&["^".into()], &[], true, false)?.render(&catalogue)?;
        let json: Json = serde_json::from_str(&json).map_err(|error| error.to_string())?;
        assert_eq!(
            json.get("package")
                .and_then(|package| package.get("description"))
                .and_then(Json::as_str),
            Some(description)
        );
        Ok(())
    }
}
