//! Ceilings on numbers a test measures, kept in a file of one `name value` a line
//! beside the tests.

// Test crates use different parts of this module.
#![allow(dead_code)]

use std::collections::BTreeMap;

/// Checks each measured number against its ceiling in the file at `path`: a run
/// over a ceiling fails, and so does one under it, so that the change that lowers
/// a number lowers its ceiling in the same commit. The numbers count what no host
/// changes (the allocator's requests and bytes, units of work), so repeated runs
/// measure the same and no spread is allowed. With `WRITE_CEILINGS` set, the
/// measured numbers are written as the new ceilings; `write` is the command that
/// does that for this test.
pub(crate) fn check_ceilings(path: &str, write: &str, measured: &[(&str, usize)]) {
    if std::env::var_os("WRITE_CEILINGS").is_some() {
        let text: String = measured.iter().map(|(k, v)| format!("{k} {v}\n")).collect();
        std::fs::write(path, text).unwrap();
        return;
    }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let ceilings: BTreeMap<&str, usize> = text
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(k, v)| (k, v.parse().unwrap()))
        .collect();
    let mut wrong = vec![];
    for (name, value) in measured {
        match ceilings.get(name) {
            Some(ceiling) if value == ceiling => {}
            Some(ceiling) if value > ceiling => {
                wrong.push(format!("{name}: {value} over its ceiling {ceiling}"));
            }
            Some(ceiling) => wrong.push(format!(
                "{name}: {value} under its ceiling {ceiling}; lower it with `{write}`"
            )),
            None => wrong.push(format!("{name}: no ceiling; write it with `{write}`")),
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
