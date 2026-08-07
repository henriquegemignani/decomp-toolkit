use std::{
    collections::{BTreeMap, btree_map},
    io::Write,
};

use anyhow::{Context, Result, anyhow, bail};
use typed_path::Utf8NativePath;

use crate::{util::file::buf_writer, vfs::open_file};

/// A set of `target_name = source_name` pairs, as written by `dtk match`.
///
/// Anything from a `#` onward is a comment. That makes the candidates file a
/// valid rename file too: its entries carry a trailing `# tier confidence
/// method` note and its rejected alternatives are already commented out, so a
/// reviewer can delete the lines they don't want and apply what's left.
#[derive(Debug, Default)]
pub struct Renames {
    /// Keyed by the name to replace, since that's what a symbols file is
    /// scanned by.
    entries: BTreeMap<String, String>,
}

impl Renames {
    #[allow(clippy::len_without_is_empty)] // callers only ever report the count
    pub fn len(&self) -> usize { self.entries.len() }

    /// Adds one pair, refusing only what would corrupt the set.
    ///
    /// Two entries assigning the same target name are allowed: local template
    /// instantiations legitimately repeat their mangled name across
    /// translation units — `GM8E01_00` has 12 such function names — so a set
    /// derived from a one-to-one function matching reproduces the source's own
    /// duplicates, and refusing them would drop correct renames. What actually
    /// collides two live symbols under one name is caught in [`apply_renames`]
    /// instead, where the target's existing names are visible.
    ///
    /// `origin` names the source of the pair for error messages.
    fn insert(&mut self, from: &str, to: &str, origin: &str) -> Result<()> {
        match self.entries.entry(from.to_string()) {
            btree_map::Entry::Vacant(e) => {
                e.insert(to.to_string());
            }
            btree_map::Entry::Occupied(e) => {
                bail!("{origin}: '{from}' is renamed twice, to '{}' and '{to}'.", e.get())
            }
        }
        Ok(())
    }

    pub fn from_pairs<I>(pairs: I) -> Result<Self>
    where I: IntoIterator<Item = (String, String)> {
        let mut renames = Self::default();
        for (from, to) in pairs {
            renames.insert(&from, &to, "Rename set")?;
        }
        Ok(renames)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut renames = Self::default();
        for (number, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (from, to) = line
                .split_once('=')
                .map(|(from, to)| (from.trim(), to.trim()))
                .filter(|(from, to)| !from.is_empty() && !to.is_empty())
                .with_context(|| {
                    format!("Line {}: expected `old_name = new_name`, got '{line}'", number + 1)
                })?;
            renames.insert(from, to, &format!("Line {}", number + 1))?;
        }
        Ok(renames)
    }

    pub fn read(path: &Utf8NativePath) -> Result<Self> {
        let mut file = open_file(path, true)?;
        let data = file.map()?;
        let text = std::str::from_utf8(data)
            .map_err(|e| anyhow!("Rename file is not valid UTF-8: {e}"))?;
        Self::parse(text).with_context(|| format!("While reading {path}"))
    }
}

/// What applying a rename set did, and what it declined to do.
#[derive(Debug, Default)]
pub struct RenameReport {
    pub applied: usize,
    /// Names in the rename set that the symbols file doesn't contain.
    pub missing: Vec<String>,
    /// Renames skipped because the new name is already taken by a symbol that
    /// isn't itself being renamed away.
    pub collisions: Vec<(String, String)>,
}

/// Rewrites symbol names in a symbols file, or reports what it would rewrite
/// when `dry_run` is set.
///
/// Operates on lines rather than parsing and regenerating the file, so
/// addresses, attributes, ordering and formatting all survive untouched and the
/// resulting diff shows only the names that changed.
pub fn apply_renames(
    path: &Utf8NativePath,
    renames: &Renames,
    dry_run: bool,
) -> Result<RenameReport> {
    let text = {
        let mut file = open_file(path, true)?;
        let data = file.map()?;
        std::str::from_utf8(data)
            .map_err(|e| anyhow!("Symbols file is not valid UTF-8: {e}"))?
            .to_string()
    };

    // A name is only free if nothing keeps it. Symbols being renamed away are
    // releasing theirs, so they don't block anyone.
    let mut taken: BTreeMap<&str, ()> = BTreeMap::new();
    for line in text.lines() {
        if let Some(name) = symbol_name(line) {
            if !renames.entries.contains_key(name) {
                taken.insert(name, ());
            }
        }
    }

    let mut report = RenameReport::default();
    let mut seen: BTreeMap<&str, ()> = BTreeMap::new();
    let mut out = String::with_capacity(text.len());

    // Split rather than `lines()` so the original trailing newline (or its
    // absence) is preserved, along with any CRLF, which rides on each line.
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let Some(name) = symbol_name(line) else {
            out.push_str(line);
            continue;
        };
        let Some(new_name) = renames.entries.get(name) else {
            out.push_str(line);
            continue;
        };
        if taken.contains_key(new_name.as_str()) {
            report.collisions.push((name.to_string(), new_name.clone()));
            out.push_str(line);
            continue;
        }
        seen.insert(name, ());
        out.push_str(new_name);
        out.push_str(&line[name.len()..]);
        report.applied += 1;
    }

    report.missing = renames
        .entries
        .keys()
        .filter(|name| !seen.contains_key(name.as_str()))
        .filter(|name| !report.collisions.iter().any(|(from, _)| from == *name))
        .cloned()
        .collect();

    if report.applied > 0 && !dry_run {
        let mut file = buf_writer(path)?;
        file.write_all(out.as_bytes())?;
        file.flush()?;
    }
    Ok(report)
}

/// The symbol name a symbols-file line declares, if it declares one.
///
/// Lines look like `name = .section:0x80000000; // attrs`.
fn symbol_name(line: &str) -> Option<&str> {
    let name = line.split_once('=')?.0.trim_end();
    // Preserve leading whitespace by requiring the name to start the line;
    // symbols files don't indent, and anything that does isn't a symbol.
    (!name.is_empty() && !name.starts_with([' ', '\t', '#', '/'])).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_pairs() {
        let renames = Renames::parse("fn_8000 = Foo\nfn_8004 = Bar\n").unwrap();
        assert_eq!(renames.len(), 2);
        assert_eq!(renames.entries["fn_8000"], "Foo");
    }

    #[test]
    fn accepts_a_candidates_file_verbatim() {
        // The candidates file's metadata is a trailing comment and its rejected
        // alternatives are whole-line comments, so it parses as a rename set.
        let renames = Renames::parse(
            "# Candidate names: a -> b\n\
             \n\
             fn_80179620 = RenderMotionBlur__17CPlasmaProjectileCFv  # probable 0.86 call-site\n\
             #       alt = FromEnum__12CPASAnimParmFi  # alternative, 20% as strong\n",
        )
        .unwrap();
        assert_eq!(renames.len(), 1);
        assert_eq!(renames.entries["fn_80179620"], "RenderMotionBlur__17CPlasmaProjectileCFv");
    }

    #[test]
    fn allows_two_functions_to_share_a_name() {
        // Local template instantiations repeat across translation units, so a
        // rename set derived from a real binary contains these legitimately.
        let renames = Renames::parse("fn_8000 = Foo\nfn_8004 = Foo\n").unwrap();
        assert_eq!(renames.len(), 2);
        assert_eq!(renames.entries["fn_8000"], "Foo");
        assert_eq!(renames.entries["fn_8004"], "Foo");
    }

    #[test]
    fn rejects_one_function_renamed_twice() {
        let err = Renames::parse("fn_8000 = Foo\nfn_8000 = Bar\n").unwrap_err();
        assert!(err.to_string().contains("renamed twice"), "{err}");
    }

    #[test]
    fn rejects_a_malformed_line() {
        assert!(Renames::parse("this is not a pair\n").is_err());
        assert!(Renames::parse("fn_8000 =\n").is_err());
    }

    #[test]
    fn reads_symbol_names_but_not_comments() {
        assert_eq!(
            symbol_name("__start = .init:0x80003140; // type:function size:0x138"),
            Some("__start")
        );
        assert_eq!(symbol_name("@100 = .text:0x80004000;"), Some("@100"));
        assert_eq!(symbol_name("// a comment = with an equals"), None);
        assert_eq!(symbol_name("  indented = 1"), None);
        assert_eq!(symbol_name(""), None);
    }
}
