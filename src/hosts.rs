//! List aliases without evaluating SSH configuration or running config helpers.
use std::{
    collections::{BTreeSet, HashSet},
    ffi::{CStr, CString, OsStr},
    fs,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use nix::{
    libc,
    unistd::{Uid, User},
};

/// Read the current user's config, using the passwd home just as OpenSSH does.
pub fn list() -> Result<Vec<String>> {
    let home = User::from_uid(Uid::current())?
        .context("current uid has no passwd entry")?
        .dir;
    list_from(&home.join(".ssh/config"), &home)
}

/// List aliases from a config tree with an explicit home (also useful for fixtures).
/// Relative includes always resolve against `home/.ssh`, not the including file.
pub fn list_from(config: &Path, home: &Path) -> Result<Vec<String>> {
    let mut reader = Reader {
        home,
        active: HashSet::new(),
        aliases: BTreeSet::new(),
    };
    reader.read(config, &mut false, 0)?;
    Ok(reader.aliases.into_iter().collect())
}

struct Reader<'a> {
    home: &'a Path,
    active: HashSet<PathBuf>,
    aliases: BTreeSet<String>,
}

impl Reader<'_> {
    fn read(&mut self, path: &Path, in_match: &mut bool, depth: usize) -> Result<()> {
        // Canonical paths catch symlink cycles; the depth limit also bounds acyclic trees.
        if depth >= 64 {
            return Ok(());
        }
        let canonical = match fs::canonicalize(path) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
        };
        if !self.active.insert(canonical.clone()) {
            return Ok(());
        }
        let result = self.read_contents(path, in_match, depth);
        self.active.remove(&canonical);
        result
    }

    fn read_contents(&mut self, path: &Path, in_match: &mut bool, depth: usize) -> Result<()> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("read SSH config {}", path.display()))?;
        for (line_number, line) in contents.lines().enumerate() {
            let line = line.trim_start();
            let end = line
                .find(|c: char| c.is_whitespace() || c == '=')
                .unwrap_or(line.len());
            let keyword = &line[..end];
            if !["host", "match", "include"]
                .iter()
                .any(|name| keyword.eq_ignore_ascii_case(name))
            {
                continue;
            }
            if keyword.eq_ignore_ascii_case("match") {
                *in_match = true;
                continue;
            }
            if *in_match && keyword.eq_ignore_ascii_case("include") {
                continue;
            }
            let args = line[end..]
                .trim_start()
                .strip_prefix('=')
                .unwrap_or(line[end..].trim_start());
            let args =
                words(args).with_context(|| format!("{}:{}", path.display(), line_number + 1))?;
            if keyword.eq_ignore_ascii_case("host") {
                *in_match = false;
                self.aliases.extend(args.into_iter().filter(|alias| {
                    !alias.is_empty()
                        && !alias.contains(['*', '?', '!'])
                        && !alias.chars().any(|c| c.is_whitespace() || c.is_control())
                }));
            } else {
                for pattern in args {
                    let expanded = expand_home(&pattern, self.home)?;
                    let pattern = if expanded.is_absolute() {
                        expanded
                    } else {
                        self.home.join(".ssh").join(expanded)
                    };
                    for included in glob(&pattern)? {
                        // OpenSSH restores the caller's block state after Include.
                        let mut included_match = *in_match;
                        self.read(&included, &mut included_match, depth + 1)?;
                    }
                }
            }
        }
        Ok(())
    }
}

// Only lex directives we use: unknown options (including executable ones) stay inert.
fn words(input: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    for c in input.chars() {
        if escaped {
            word.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
            started = true;
        } else if quote == Some(c) {
            quote = None;
        } else if quote.is_some() {
            word.push(c);
        } else if c == '"' || c == '\'' {
            quote = Some(c);
            started = true;
        } else if c == '#' && !started {
            break;
        } else if c.is_whitespace() {
            if started {
                words.push(std::mem::take(&mut word));
                started = false;
            }
        } else {
            word.push(c);
            started = true;
        }
    }
    if quote.is_some() || escaped {
        bail!("unterminated quote or escape in SSH config");
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

fn expand_home(pattern: &str, home: &Path) -> Result<PathBuf> {
    let Some(rest) = pattern.strip_prefix('~') else {
        return Ok(PathBuf::from(pattern));
    };
    let (name, suffix) = rest.split_once('/').unwrap_or((rest, ""));
    let base = if name.is_empty() {
        home.to_path_buf()
    } else {
        User::from_name(name)?
            .with_context(|| format!("unknown user in Include: {name}"))?
            .dir
    };
    Ok(base.join(suffix))
}

fn glob(pattern: &Path) -> Result<Vec<PathBuf>> {
    let pattern = CString::new(pattern.as_os_str().as_bytes()).context("NUL in Include path")?;
    // glob owns the result storage, which must be released even on a partial failure.
    let mut result = unsafe { std::mem::zeroed::<libc::glob_t>() };
    let status = unsafe { libc::glob(pattern.as_ptr(), 0, None, &mut result) };
    let paths = if status == 0 {
        (0..result.gl_pathc)
            .map(|index| {
                // Successful glob guarantees gl_pathc valid, NUL-terminated path pointers.
                let bytes = unsafe { CStr::from_ptr(*result.gl_pathv.add(index)) }.to_bytes();
                PathBuf::from(OsStr::from_bytes(bytes))
            })
            .collect()
    } else {
        Vec::new()
    };
    unsafe { libc::globfree(&mut result) };
    if status != 0 && status != libc::GLOB_NOMATCH {
        bail!("Include glob failed (code {status})");
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture(files: &[(&str, &str)]) -> TempDir {
        let home = tempfile::tempdir().unwrap();
        for (name, contents) in files {
            let path = home.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        home
    }

    #[test]
    fn concrete_aliases_are_sorted_and_unique() {
        let home = fixture(&[(
            ".ssh/config",
            "Host z a b a *.example a? !neg a!b\nhOsT= c # comment\nHost *\n",
        )]);
        assert_eq!(
            list_from(&home.path().join(".ssh/config"), home.path()).unwrap(),
            ["a", "b", "c", "z"]
        );
    }

    #[test]
    fn includes_expand_globs_tildes_and_home_relative_paths() {
        let home = fixture(&[
            (
                ".ssh/config",
                "Host root\nInclude = conf.d/* ~/abs missing* absent\n",
            ),
            (".ssh/conf.d/a", "Host a\nInclude nested\n"),
            (".ssh/conf.d/b", "Host b a\n"),
            (".ssh/nested", "Host nested\n"),
            ("abs", "Host tilde\n"),
        ]);
        assert_eq!(
            list_from(&home.path().join(".ssh/config"), home.path()).unwrap(),
            ["a", "b", "nested", "root", "tilde"]
        );
    }

    #[test]
    fn match_includes_are_inert_and_host_resumes_listing() {
        let home = fixture(&[
            (
                ".ssh/config",
                "Host before\nMatch exec \"touch never\"\nInclude ignored\nHost after\nInclude resumed\n",
            ),
            (".ssh/ignored", "Host hidden\n"),
            (".ssh/resumed", "Host visible\n"),
        ]);
        assert_eq!(
            list_from(&home.path().join(".ssh/config"), home.path()).unwrap(),
            ["after", "before", "visible"]
        );
    }

    #[test]
    fn cycles_including_symlinks_terminate() {
        let home = fixture(&[
            (".ssh/config", "Host a\nInclude child\n"),
            (".ssh/child", "Host b\nInclude config link\n"),
        ]);
        std::os::unix::fs::symlink("config", home.path().join(".ssh/link")).unwrap();
        assert_eq!(
            list_from(&home.path().join(".ssh/config"), home.path()).unwrap(),
            ["a", "b"]
        );
    }

    #[test]
    fn quoting_comments_equals_and_absolute_includes() {
        let home = fixture(&[(
            ".ssh/with space#part",
            "Host \"quoted\" 'single' escaped\\#hash # ignored\n",
        )]);
        let config = home.path().join(".ssh/config");
        fs::write(
            &config,
            format!(
                "Include=\"{}\"\nProxyCommand sh -c 'exit 1'\n",
                home.path().join(".ssh/with space#part").display()
            ),
        )
        .unwrap();
        assert_eq!(
            list_from(&config, home.path()).unwrap(),
            ["escaped#hash", "quoted", "single"]
        );
    }

    #[test]
    fn unquoted_hash_inside_tokens_is_literal() {
        let home = fixture(&[
            (
                ".ssh/config",
                "Host foo#bar # comment\nInclude hash#part # comment\n",
            ),
            (".ssh/hash#part", "Host included\n"),
        ]);
        assert_eq!(
            list_from(&home.path().join(".ssh/config"), home.path()).unwrap(),
            ["foo#bar", "included"]
        );
    }

    #[test]
    fn missing_config_is_empty_but_invalid_config_is_an_error() {
        let home = fixture(&[]);
        let config = home.path().join("config");
        assert!(list_from(&config, home.path()).unwrap().is_empty());
        fs::write(&config, "Host \"unfinished\n").unwrap();
        assert!(
            list_from(&config, home.path())
                .unwrap_err()
                .to_string()
                .contains("config:1")
        );
        assert!(list_from(home.path(), home.path()).is_err());
    }

    #[test]
    fn include_state_is_scoped_and_repeated_files_are_not_globally_skipped() {
        let home = fixture(&[
            (
                ".ssh/config",
                "Include child\nInclude ignored\nHost reset\nInclude child\nHost end\n",
            ),
            (".ssh/child", "Host child\nMatch all\n"),
            (".ssh/ignored", "Host hidden\n"),
        ]);
        assert_eq!(
            list_from(&home.path().join(".ssh/config"), home.path()).unwrap(),
            ["child", "end", "hidden", "reset"]
        );
    }
}
