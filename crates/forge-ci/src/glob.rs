//! GitHub's filter patterns for branches, tags and paths: `*` matches anything but `/`,
//! `**` matches anything, `**/` may match nothing, `?` makes the previous character
//! optional, `+` repeats it, `[a-z]` is a set, and `!` in front of a pattern in a list
//! excludes what it matches.

/// Whether `name` passes a list of patterns, where later patterns override earlier ones.
pub fn matches_list(patterns: &[String], name: &str) -> bool {
    let mut matched = false;
    for pattern in patterns {
        if let Some(negated) = pattern.strip_prefix('!') {
            if matched && matches(negated, name) {
                matched = false;
            }
        } else if !matched && matches(pattern, name) {
            matched = true;
        }
    }
    matched
}

pub fn matches(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    match_at(&pattern, &name)
}

/// One element of a pattern and how many times it may repeat.
enum Atom {
    Char(char),
    Set(Vec<(char, char)>, bool),
    Star,
    DoubleStar,
}

fn atom(pattern: &[char]) -> (Atom, usize) {
    match pattern[0] {
        '*' if pattern.get(1) == Some(&'*') => {
            // `**/` can stand for no directories at all.
            if pattern.get(2) == Some(&'/') {
                (Atom::DoubleStar, 3)
            } else {
                (Atom::DoubleStar, 2)
            }
        }
        '*' => (Atom::Star, 1),
        '\\' if pattern.len() > 1 => (Atom::Char(pattern[1]), 2),
        '[' => {
            let mut i = 1;
            let negated = matches!(pattern.get(1), Some('!') | Some('^'));
            if negated {
                i += 1;
            }
            let mut ranges = Vec::new();
            while i < pattern.len() && (pattern[i] != ']' || ranges.is_empty()) {
                let start = pattern[i];
                if pattern.get(i + 1) == Some(&'-') && pattern.get(i + 2).is_some_and(|c| *c != ']')
                {
                    ranges.push((start, pattern[i + 2]));
                    i += 3;
                } else {
                    ranges.push((start, start));
                    i += 1;
                }
            }
            if i >= pattern.len() {
                // No closing bracket: a literal `[`.
                return (Atom::Char('['), 1);
            }
            (Atom::Set(ranges, negated), i + 1)
        }
        c => (Atom::Char(c), 1),
    }
}

fn single(atom: &Atom, c: char) -> bool {
    match atom {
        Atom::Char(expected) => *expected == c,
        Atom::Set(ranges, negated) => {
            ranges.iter().any(|(a, b)| (*a..=*b).contains(&c)) != *negated
        }
        Atom::Star | Atom::DoubleStar => unreachable!(),
    }
}

fn match_at(pattern: &[char], name: &[char]) -> bool {
    if pattern.is_empty() {
        return name.is_empty();
    }
    let (atom, len) = atom(pattern);
    let rest = &pattern[len..];
    match atom {
        Atom::Star => (0..=name.len())
            .take_while(|&i| i == 0 || name[i - 1] != '/')
            .any(|i| match_at(rest, &name[i..])),
        Atom::DoubleStar => {
            let slash = len == 3;
            if slash && match_at(rest, name) {
                return true;
            }
            (0..=name.len()).any(|i| {
                if slash {
                    // `**/x`: what's skipped must end at a `/`.
                    i > 0 && name[i - 1] == '/' && match_at(rest, &name[i..])
                } else {
                    match_at(rest, &name[i..])
                }
            })
        }
        atom => match rest.first() {
            Some('?') => {
                match_at(&rest[1..], name)
                    || (!name.is_empty()
                        && single(&atom, name[0])
                        && match_at(&rest[1..], &name[1..]))
            }
            Some('+') => {
                let mut i = 0;
                while i < name.len() && single(&atom, name[i]) {
                    i += 1;
                    if match_at(&rest[1..], &name[i..]) {
                        return true;
                    }
                }
                false
            }
            _ => !name.is_empty() && single(&atom, name[0]) && match_at(rest, &name[1..]),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_github_filter_syntax() {
        assert!(matches("main", "main"));
        assert!(!matches("main", "mainline"));
        assert!(matches("feature/*", "feature/login"));
        assert!(!matches("feature/*", "feature/login/form"));
        assert!(matches("feature/**", "feature/login/form"));
        assert!(matches("**/README.md", "README.md"));
        assert!(matches("**/README.md", "docs/deep/README.md"));
        assert!(matches("**.js", "app/index.js"));
        assert!(matches("*.js", "index.js"));
        assert!(!matches("*.js", "app/index.js"));
        assert!(matches("v[0-9]+.*", "v12.3"));
        assert!(!matches("v[0-9]+.*", "v.3"));
        assert!(matches("colou?r", "color"));
        assert!(matches("colou?r", "colour"));
        assert!(matches("docs/**", "docs/a/b.md"));
    }

    #[test]
    fn later_patterns_win() {
        let patterns: Vec<String> = ["releases/**", "!releases/**-alpha"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(matches_list(&patterns, "releases/10"));
        assert!(!matches_list(&patterns, "releases/10-alpha"));
        assert!(!matches_list(&patterns, "main"));
    }
}
