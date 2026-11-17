//! The GNU Make a large driver's Kbuild makefiles are written in, evaluated as
//! Make would: enough to compose the driver's object list, include paths and
//! per-object flags from its unchanged makefiles under one Kconfig. Anything
//! outside what is implemented here is an error where it is reached, never a
//! guess; text in an inactive conditional is skipped unread, as Make skips it.
//!
//! Implemented: comments and line continuations; `=`, `:=`, `+=` and `?=`
//! with recursively and simply expanded variables, also under computed names;
//! `$(name)` references; `ifdef`, `ifndef`, `ifeq (a,b)`, `ifneq (a,b)`,
//! `else` and `endif`; `include`; the functions `addprefix`, `addsuffix`,
//! `filter`, `patsubst`, `foreach`, `eval` and `info`; Kbuild's `test-lt`
//! through `call`; and `$(shell find <directory> -name '*.c' -type f)`,
//! answered from the file system in sorted order.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

/// Bounds on what one driver's makefiles may hold.
const MAX_FILES: usize = 256;
const MAX_DEPTH: usize = 64;
const MAX_BYTES: usize = 1 << 20;

#[derive(Clone, Copy, PartialEq)]
enum Flavor {
    Simple,
    Recursive,
}

/// The variables a run of makefiles defined.
#[derive(Default)]
pub struct Make {
    variables: BTreeMap<String, (Flavor, String)>,
    /// Every makefile read, in order.
    pub files: Vec<PathBuf>,
    /// Variables referenced while undefined, which Make expands to nothing.
    pub undefined: BTreeSet<String>,
}

impl Make {
    /// Defines `name` as a simply expanded variable.
    pub fn set(&mut self, name: &str, value: &str) {
        self.variables
            .insert(name.to_owned(), (Flavor::Simple, value.to_owned()));
    }

    /// Defines the `CONFIG_` symbols of a Kconfig header
    /// (`#define CONFIG_<X> <value>`) as Kbuild does: `y` for a boolean that
    /// is set, the value's text for any other.
    pub fn configure(&mut self, autoconf: &str) -> Result<(), String> {
        for line in autoconf.lines() {
            let Some(definition) = line.strip_prefix("#define CONFIG_") else {
                continue;
            };
            let (name, value) = definition.split_once(' ').unwrap_or((definition, ""));
            let name = format!("CONFIG_{name}");
            if name.ends_with("_MODULE") {
                return Err(format!("{name} selects a module"));
            }
            let value = match value.trim() {
                "1" => "y",
                value => value,
            };
            if self.variables.contains_key(&name) {
                return Err(format!("{name} is defined twice"));
            }
            self.set(&name, value);
        }
        Ok(())
    }

    /// Reads the makefile at `path` and those it includes.
    pub fn include(&mut self, path: &Path) -> Result<(), String> {
        self.include_at(path, 0)
    }

    /// The words of `name`'s expanded value; none when it is undefined.
    pub fn words(&mut self, name: &str) -> Result<Vec<String>, String> {
        let Some((flavor, value)) = self.variables.get(name).cloned() else {
            return Ok(Vec::new());
        };
        let value = match flavor {
            Flavor::Simple => value,
            Flavor::Recursive => self.expand(&value, 0)?,
        };
        Ok(value.split_whitespace().map(str::to_owned).collect())
    }

    /// Every defined variable whose name starts with `prefix`, as the rest of
    /// its name and its words.
    pub fn prefixed(&mut self, prefix: &str) -> Result<Vec<(String, Vec<String>)>, String> {
        let names = self
            .variables
            .keys()
            .filter(|name| name.starts_with(prefix))
            .cloned()
            .collect::<Vec<_>>();
        names
            .into_iter()
            .map(|name| Ok((name[prefix.len()..].to_owned(), self.words(&name)?)))
            .collect()
    }

    fn include_at(&mut self, path: &Path, depth: usize) -> Result<(), String> {
        if depth > MAX_DEPTH || self.files.len() == MAX_FILES {
            return Err(format!("{} nests past the makefile bounds", path.display()));
        }
        let text =
            fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
        if text.len() > MAX_BYTES {
            return Err(format!("{} is too large", path.display()));
        }
        self.files.push(path.to_owned());
        self.evaluate(&text, depth)
            .map_err(|error| format!("{}: {error}", path.display()))
    }

    /// Evaluates makefile text: logical lines under its conditionals.
    fn evaluate(&mut self, text: &str, depth: usize) -> Result<(), String> {
        // Each open conditional: whether its branch is active, whether a
        // branch of it was already taken, and whether its parent is active.
        let mut conditionals: Vec<(bool, bool, bool)> = Vec::new();
        for (number, line) in logical_lines(text) {
            let at = |error: String| format!("line {number}: {error}");
            let active = conditionals.last().is_none_or(|(active, ..)| *active);
            let (keyword, rest) = line
                .split_once(char::is_whitespace)
                .map_or((line.as_str(), ""), |(keyword, rest)| {
                    (keyword, rest.trim())
                });
            match keyword {
                "ifdef" | "ifndef" | "ifeq" | "ifneq" => {
                    let holds = if active {
                        self.condition(keyword, rest, depth).map_err(at)?
                    } else {
                        false
                    };
                    conditionals.push((active && holds, holds, active));
                }
                "else" => {
                    if !rest.is_empty() {
                        return Err(at(format!("unsupported `else {rest}`")));
                    }
                    let Some((active, taken, parent)) = conditionals.last_mut() else {
                        return Err(at("else without a conditional".into()));
                    };
                    *active = *parent && !*taken;
                    *taken = true;
                }
                "endif" => {
                    if conditionals.pop().is_none() {
                        return Err(at("endif without a conditional".into()));
                    }
                }
                _ if !active => {}
                "include" => {
                    let paths = self.expand(rest, depth).map_err(at)?;
                    for path in paths.split_whitespace() {
                        self.include_at(Path::new(path), depth + 1)?;
                    }
                }
                _ => self.statement(&line, depth).map_err(at)?,
            }
        }
        if conditionals.is_empty() {
            Ok(())
        } else {
            Err("a conditional without endif".into())
        }
    }

    fn condition(&mut self, keyword: &str, rest: &str, depth: usize) -> Result<bool, String> {
        match keyword {
            "ifdef" | "ifndef" => {
                let name = self.expand(rest, depth)?;
                let name = name.trim();
                // Make asks whether the variable has a non-empty value,
                // without expanding it.
                let defined = self
                    .variables
                    .get(name)
                    .is_some_and(|(_, value)| !value.is_empty());
                Ok(defined == (keyword == "ifdef"))
            }
            _ => {
                let inner = rest
                    .strip_prefix('(')
                    .and_then(|rest| rest.strip_suffix(')'))
                    .ok_or_else(|| format!("unsupported condition `{rest}`"))?;
                let arguments = arguments(inner)?;
                let [left, right] = arguments.as_slice() else {
                    return Err(format!("`{rest}` does not compare two values"));
                };
                let left = self.expand(left, depth)?;
                let right = self.expand(right, depth)?;
                Ok((left.trim() == right.trim()) == (keyword == "ifeq"))
            }
        }
    }

    /// An assignment, or a line whose expansion is only for its effects.
    fn statement(&mut self, line: &str, depth: usize) -> Result<(), String> {
        let Some((at, operator)) = assignment(line) else {
            let expanded = self.expand(line, depth)?;
            return if expanded.trim().is_empty() {
                Ok(())
            } else {
                Err(format!("unsupported statement `{line}`"))
            };
        };
        let name = self.expand(line[..at].trim(), depth)?;
        let name = name.trim();
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Err(format!("`{line}` assigns to no single variable"));
        }
        let value = line[at + operator.len()..].trim();
        match operator {
            ":=" => {
                let value = self.expand(value, depth)?;
                self.variables
                    .insert(name.to_owned(), (Flavor::Simple, value));
            }
            "=" => {
                self.variables
                    .insert(name.to_owned(), (Flavor::Recursive, value.to_owned()));
            }
            "?=" => {
                if !self.variables.contains_key(name) {
                    self.variables
                        .insert(name.to_owned(), (Flavor::Recursive, value.to_owned()));
                }
            }
            _ => {
                let flavor = self
                    .variables
                    .get(name)
                    .map_or(Flavor::Recursive, |(flavor, _)| *flavor);
                let value = match flavor {
                    Flavor::Simple => self.expand(value, depth)?,
                    Flavor::Recursive => value.to_owned(),
                };
                let entry = self
                    .variables
                    .entry(name.to_owned())
                    .or_insert((flavor, String::new()));
                if !entry.1.is_empty() && !value.is_empty() {
                    entry.1.push(' ');
                }
                entry.1.push_str(&value);
            }
        }
        Ok(())
    }

    /// Expands every `$(...)` of `text`.
    fn expand(&mut self, text: &str, depth: usize) -> Result<String, String> {
        if depth > MAX_DEPTH {
            return Err("expansion nests past the makefile bounds".into());
        }
        let mut output = String::new();
        let mut rest = text;
        while let Some(at) = rest.find('$') {
            output.push_str(&rest[..at]);
            let after = &rest[at + 1..];
            if let Some(after) = after.strip_prefix('$') {
                output.push('$');
                rest = after;
                continue;
            }
            let Some(inner) = after.strip_prefix('(') else {
                return Err(format!("unsupported reference in `{text}`"));
            };
            let end = closing(inner).ok_or_else(|| format!("unbalanced `{text}`"))?;
            output.push_str(&self.reference(&inner[..end], depth + 1)?);
            rest = &inner[end + 1..];
        }
        output.push_str(rest);
        Ok(output)
    }

    /// The value of what stands inside one `$(...)`.
    fn reference(&mut self, inner: &str, depth: usize) -> Result<String, String> {
        let Some((function, rest)) = inner.split_once(char::is_whitespace) else {
            let name = self.expand(inner, depth)?;
            return match self.variables.get(&name).cloned() {
                Some((Flavor::Simple, value)) => Ok(value),
                Some((Flavor::Recursive, value)) => self.expand(&value, depth),
                None => {
                    self.undefined.insert(name);
                    Ok(String::new())
                }
            };
        };
        let raw = arguments(rest)?;
        let expanded = |make: &mut Self, index: usize| -> Result<String, String> {
            let argument = raw
                .get(index)
                .ok_or_else(|| format!("`{function}` lacks an argument"))?;
            make.expand(argument, depth)
        };
        let count = |wanted: usize| {
            if raw.len() == wanted {
                Ok(())
            } else {
                Err(format!("`{function}` takes {wanted} arguments"))
            }
        };
        match function {
            "addprefix" | "addsuffix" => {
                count(2)?;
                let affix = expanded(self, 0)?;
                let affix = affix.trim();
                let words = expanded(self, 1)?;
                Ok(join(words.split_whitespace().map(|word| {
                    if function == "addprefix" {
                        format!("{affix}{word}")
                    } else {
                        format!("{word}{affix}")
                    }
                })))
            }
            "filter" => {
                count(2)?;
                let patterns = expanded(self, 0)?;
                let words = expanded(self, 1)?;
                Ok(join(
                    words
                        .split_whitespace()
                        .filter(|word| {
                            patterns
                                .split_whitespace()
                                .any(|pattern| stem(pattern, word).is_some())
                        })
                        .map(str::to_owned),
                ))
            }
            "patsubst" => {
                count(3)?;
                let pattern = expanded(self, 0)?;
                let replacement = expanded(self, 1)?;
                let words = expanded(self, 2)?;
                Ok(join(words.split_whitespace().map(|word| {
                    match stem(pattern.trim(), word) {
                        Some(stem) => replacement.trim().replacen('%', stem, 1),
                        None => word.to_owned(),
                    }
                })))
            }
            "foreach" => {
                count(3)?;
                let variable = expanded(self, 0)?;
                let variable = variable.trim().to_owned();
                let words = expanded(self, 1)?;
                let saved = self.variables.get(&variable).cloned();
                let mut results = Vec::new();
                for word in words.split_whitespace() {
                    self.set(&variable, word);
                    results.push(self.expand(&raw[2], depth)?);
                }
                match saved {
                    Some(saved) => self.variables.insert(variable, saved),
                    None => self.variables.remove(&variable),
                };
                Ok(join(results.into_iter()))
            }
            "eval" => {
                let text = self.expand(rest, depth)?;
                self.evaluate(&text, depth)?;
                Ok(String::new())
            }
            "info" => Ok(String::new()),
            "call" => {
                let name = expanded(self, 0)?;
                if name.trim() != "test-lt" {
                    return Err(format!("unsupported `$(call {})`", name.trim()));
                }
                count(3)?;
                // Kbuild's `test-lt`: `y` when the shell's integer test
                // holds, nothing when it fails or an operand is no integer.
                let left = expanded(self, 1)?.trim().parse::<i64>();
                let right = expanded(self, 2)?.trim().parse::<i64>();
                Ok(match (left, right) {
                    (Ok(left), Ok(right)) if left < right => "y".into(),
                    _ => String::new(),
                })
            }
            "shell" => {
                let command = self.expand(rest, depth)?;
                let directory = command
                    .trim()
                    .strip_prefix("find ")
                    .and_then(|rest| rest.strip_suffix(" -name '*.c' -type f"))
                    .filter(|directory| !directory.contains(char::is_whitespace))
                    .ok_or_else(|| format!("unsupported `$(shell {})`", command.trim()))?;
                let mut found = Vec::new();
                find_c(Path::new(directory), &mut found)?;
                found.sort();
                Ok(join(found.into_iter()))
            }
            function => Err(format!("unsupported function `{function}`")),
        }
    }
}

fn join(words: impl Iterator<Item = String>) -> String {
    words.collect::<Vec<_>>().join(" ")
}

/// The `.c` files below `directory`, as `find` names them.
fn find_c(directory: &Path, found: &mut Vec<String>) -> Result<(), String> {
    let entries =
        fs::read_dir(directory).map_err(|error| format!("{}: {error}", directory.display()))?;
    for entry in entries {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.is_dir() {
            find_c(&path, found)?;
        } else if path.extension().is_some_and(|extension| extension == "c") {
            found.push(path.display().to_string());
        }
    }
    Ok(())
}

/// What `%` stands for when `pattern` matches `word`; a pattern without `%`
/// matches only itself.
fn stem<'a>(pattern: &str, word: &'a str) -> Option<&'a str> {
    match pattern.split_once('%') {
        Some((prefix, suffix)) => word
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(suffix)),
        None => (pattern == word).then_some(""),
    }
}

/// The index of the `)` that closes a reference whose `(` was just read.
fn closing(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (index, byte) in text.bytes().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' if depth == 0 => return Some(index),
            b')' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// A function's comma-separated arguments, unexpanded; commas inside nested
/// references belong to those.
fn arguments(text: &str) -> Result<Vec<String>, String> {
    let mut arguments = vec![String::new()];
    let mut depth = 0usize;
    for character in text.chars() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| format!("unbalanced `{text}`"))?;
            }
            ',' if depth == 0 => {
                arguments.push(String::new());
                continue;
            }
            _ => {}
        }
        arguments.last_mut().expect("one argument").push(character);
    }
    Ok(arguments)
}

/// Where a line's assignment operator stands outside any reference, and the
/// operator.
fn assignment(line: &str) -> Option<(usize, &'static str)> {
    let bytes = line.as_bytes();
    let mut depth = 0usize;
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b'=' if depth == 0 => {
                return Some(match index.checked_sub(1).map(|before| bytes[before]) {
                    Some(b':') => (index - 1, ":="),
                    Some(b'+') => (index - 1, "+="),
                    Some(b'?') => (index - 1, "?="),
                    _ => (index, "="),
                });
            }
            _ => {}
        }
    }
    None
}

/// The text's logical lines with the number of their first line: comments
/// removed, continuations joined by one space and blank lines dropped.
fn logical_lines(text: &str) -> Vec<(usize, String)> {
    let mut lines = Vec::new();
    let mut pending: Option<(usize, String)> = None;
    for (index, line) in text.lines().enumerate() {
        let line = line.split_once('#').map_or(line, |(code, _)| code);
        let (first, mut logical) = pending.take().unwrap_or((index + 1, String::new()));
        let (part, continued) = match line.trim_end().strip_suffix('\\') {
            Some(part) => (part, true),
            None => (line, false),
        };
        if !logical.is_empty() {
            logical.push(' ');
        }
        logical.push_str(part.trim());
        if continued {
            pending = Some((first, logical));
        } else if !logical.trim().is_empty() {
            lines.push((first, logical.trim().to_owned()));
        }
    }
    if let Some((first, logical)) = pending
        && !logical.trim().is_empty()
    {
        lines.push((first, logical.trim().to_owned()));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str) -> Make {
        let mut make = Make::default();
        make.configure("#define CONFIG_ON 1\n#define CONFIG_LIMIT 2048\n")
            .unwrap();
        make.evaluate(text, 0).unwrap();
        make
    }

    #[test]
    fn recursive_variables_expand_at_use_and_simple_ones_when_assigned() {
        let mut make = run("A = $(B) tail\nS := $(B)\nB = late\nS += $(B)\nA += more\n");
        assert_eq!(make.words("A").unwrap(), ["late", "tail", "more"]);
        assert_eq!(make.words("S").unwrap(), ["late"]);
        assert!(make.undefined.contains("B"));
        assert_eq!(make.words("none").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn objects_compose_under_computed_names_and_config_conditions() {
        let mut make = run("drv-y := a.o\n\
             drv-$(CONFIG_ON) += b.o \\\n\tc.o # trailing\n\
             drv-$(CONFIG_OFF) += never.o\n\
             ifdef CONFIG_ON\ndrv-y += d.o\nelse\ndrv-y += e.o\nendif\n\
             ifneq ($(CONFIG_OFF),)\ndrv-y += f.o\nendif\n\
             ifeq ($(CONFIG_ON)$(CONFIG_OFF),y)\ndrv-y += g.o\nendif\n\
             ifndef CONFIG_OFF\n  ifdef CONFIG_OFF\n  drv-y += h.o\n  else\n  drv-y += i.o\n  endif\nendif\n");
        assert_eq!(
            make.words("drv-y").unwrap(),
            ["a.o", "b.o", "c.o", "d.o", "g.o", "i.o"]
        );
        assert_eq!(make.words("drv-").unwrap(), ["never.o"]);
    }

    #[test]
    fn an_inactive_branch_is_not_read() {
        let mut make = run(
            "ifdef CONFIG_OFF\n$(error stop)\nX := $(shell rm -rf /)\nelse\nX := kept\nendif\n",
        );
        assert_eq!(make.words("X").unwrap(), ["kept"]);
    }

    #[test]
    fn list_functions_follow_make() {
        let mut make = run("P = ../dc\nL = a.o b.o\n\
             F := $(addprefix $(P)/, $(L)) $(addsuffix /Makefile,$(addprefix x/,one two))\n\
             G := $(filter y,n y m) $(filter %.c,a.c b.h c.c)\n\
             H := $(patsubst src/%,dc/%,$(patsubst %.c,%.o,src/a.c src/sub/b.c other.c))\n");
        assert_eq!(
            make.words("F").unwrap(),
            ["../dc/a.o", "../dc/b.o", "x/one/Makefile", "x/two/Makefile"]
        );
        assert_eq!(make.words("G").unwrap(), ["y", "a.c", "c.c"]);
        assert_eq!(
            make.words("H").unwrap(),
            ["dc/a.o", "dc/sub/b.o", "other.o"]
        );
    }

    #[test]
    fn foreach_and_eval_define_one_variable_per_word() {
        let mut make = run("FLAGS := -msse\nOBJECTS := a.o b.o\n\
             $(foreach obj,$(OBJECTS),$(eval CFLAGS_$(obj) := $(FLAGS)))\n");
        assert_eq!(
            make.prefixed("CFLAGS_").unwrap(),
            [
                ("a.o".to_owned(), vec!["-msse".to_owned()]),
                ("b.o".to_owned(), vec!["-msse".to_owned()])
            ]
        );
        assert!(!make.variables.contains_key("obj"));
    }

    #[test]
    fn test_lt_compares_integers_and_fails_closed() {
        let mut make = run("A := $(call test-lt, $(CONFIG_LIMIT), 3072)\n\
             B := $(call test-lt, 4096, 3072)\nC := $(call test-lt, $(CONFIG_UNSET), 3072)\n");
        assert_eq!(make.words("A").unwrap(), ["y"]);
        assert!(make.words("B").unwrap().is_empty());
        assert!(make.words("C").unwrap().is_empty());
    }

    #[test]
    fn includes_and_the_c_file_search_read_the_tree() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path();
        fs::create_dir_all(root.join("lib/deep")).unwrap();
        fs::write(root.join("lib/b.c"), "").unwrap();
        fs::write(root.join("lib/deep/a.c"), "").unwrap();
        fs::write(root.join("lib/skip.h"), "").unwrap();
        fs::write(root.join("lib/Makefile"), "LIB := from-lib\n").unwrap();
        fs::write(
            root.join("Makefile"),
            "include $(src)/lib/Makefile\nC := $(patsubst $(src)/%,%,$(shell find $(src)/lib -name '*.c' -type f))\n",
        )
        .unwrap();
        let mut make = Make::default();
        make.set("src", &root.display().to_string());
        make.include(&root.join("Makefile")).unwrap();
        assert_eq!(make.words("LIB").unwrap(), ["from-lib"]);
        assert_eq!(make.words("C").unwrap(), ["lib/b.c", "lib/deep/a.c"]);
        assert_eq!(make.files.len(), 2);
    }

    #[test]
    fn what_is_not_implemented_is_refused_where_reached() {
        for text in [
            "X := $(shell uname)\n",
            "X := $(wildcard *.c)\n",
            "X := $(call cc-option,-O2)\n",
            "X := ${BRACES}\n",
            "target: dependency\n",
            "ifdef CONFIG_ON\n",
            "endif\n",
            "else\n",
            "ifeq ($(CONFIG_ON),y)\nelse ifeq (a,b)\nendif\n",
            "X := $(addprefix only-one)\n",
            "X := $(unclosed\n",
        ] {
            let mut make = Make::default();
            make.configure("#define CONFIG_ON 1\n").unwrap();
            assert!(make.evaluate(text, 0).is_err(), "{text}");
        }
        let mut make = Make::default();
        assert!(make.configure("#define CONFIG_X_MODULE 1\n").is_err());
        assert!(
            make.configure("#define CONFIG_A 1\n#define CONFIG_A 1\n")
                .is_err()
        );
    }
}
