use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    str,
};

use anstyle::{AnsiColor, Color, Style};
use anyhow::{anyhow, Context, Result};
use indoc::indoc;
use lazy_static::lazy_static;
use regex::{
    bytes::{Regex as ByteRegex, RegexBuilder as ByteRegexBuilder},
    Regex,
};
use similar::{ChangeTag, TextDiff};
use tree_sitter::{format_sexp, Language, LogType, Parser, Query};
use walkdir::WalkDir;

use super::util;

lazy_static! {
    static ref HEADER_REGEX: ByteRegex = ByteRegexBuilder::new(
        r"^(?x)
           (?P<equals>(?:=+){3,})
           (?P<suffix1>[^=\r\n][^\r\n]*)?
           \r?\n
           (?P<test_name_and_markers>(?:([^=\r\n]|\s+:)[^\r\n]*\r?\n)+)
           ===+
           (?P<suffix2>[^=\r\n][^\r\n]*)?\r?\n"
    )
    .multi_line(true)
    .build()
    .unwrap();
    static ref DIVIDER_REGEX: ByteRegex =
        ByteRegexBuilder::new(r"^(?P<hyphens>(?:-+){3,})(?P<suffix>[^-\r\n][^\r\n]*)?\r?\n")
            .multi_line(true)
            .build()
            .unwrap();
    static ref COMMENT_REGEX: Regex = Regex::new(r"(?m)^\s*;.*$").unwrap();
    static ref WHITESPACE_REGEX: Regex = Regex::new(r"\s+").unwrap();
    static ref SEXP_FIELD_REGEX: Regex = Regex::new(r" \w+: \(").unwrap();
    static ref POINT_REGEX: Regex = Regex::new(r"\s*\[\s*\d+\s*,\s*\d+\s*\]\s*").unwrap();
}

#[derive(Debug, PartialEq, Eq)]
pub enum TestEntry {
    Group {
        name: String,
        children: Vec<TestEntry>,
        file_path: Option<PathBuf>,
    },
    Example {
        name: String,
        input: Vec<u8>,
        output: String,
        header_delim_len: usize,
        divider_delim_len: usize,
        has_fields: bool,
        attributes_str: String,
        attributes: TestAttributes,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestAttributes {
    pub skip: bool,
    pub platform: bool,
    pub fail_fast: bool,
    pub error: bool,
    pub languages: Vec<Box<str>>,
}

impl Default for TestEntry {
    fn default() -> Self {
        Self::Group {
            name: String::new(),
            children: Vec::new(),
            file_path: None,
        }
    }
}

impl Default for TestAttributes {
    fn default() -> Self {
        Self {
            skip: false,
            platform: true,
            fail_fast: false,
            error: false,
            languages: vec!["".into()],
        }
    }
}

pub struct TestOptions<'a> {
    pub path: PathBuf,
    pub debug: bool,
    pub debug_graph: bool,
    pub include: Option<Regex>,
    pub exclude: Option<Regex>,
    pub update: bool,
    pub open_log: bool,
    pub languages: BTreeMap<&'a str, &'a Language>,
    pub color: bool,
    pub test_num: usize,
    pub show_fields: bool,
    pub overview_only: bool,
}

pub fn run_tests_at_path(parser: &mut Parser, opts: &mut TestOptions) -> Result<()> {
    let test_entry = parse_tests(&opts.path)?;
    let mut _log_session = None;

    if opts.debug_graph {
        _log_session = Some(util::log_graphs(parser, "log.html", opts.open_log)?);
    } else if opts.debug {
        parser.set_logger(Some(Box::new(|log_type, message| {
            if log_type == LogType::Lex {
                io::stderr().write_all(b"  ").unwrap();
            }
            writeln!(&mut io::stderr(), "{message}").unwrap();
        })));
    }

    let mut failures = Vec::new();
    let mut corrected_entries = Vec::new();
    let mut has_parse_errors = false;
    run_tests(
        parser,
        test_entry,
        opts,
        0,
        &mut failures,
        &mut corrected_entries,
        &mut has_parse_errors,
    )?;

    parser.stop_printing_dot_graphs();

    if failures.is_empty() {
        Ok(())
    } else {
        println!();

        if opts.update && !has_parse_errors {
            if failures.len() == 1 {
                println!("1 update:\n");
            } else {
                println!("{} updates:\n", failures.len());
            }

            for (i, (name, ..)) in failures.iter().enumerate() {
                println!("  {}. {name}", i + 1);
            }

            Ok(())
        } else {
            has_parse_errors = opts.update && has_parse_errors;

            if !opts.overview_only {
                if !has_parse_errors {
                    if failures.len() == 1 {
                        println!("1 failure:");
                    } else {
                        println!("{} failures:", failures.len());
                    }
                }

                if opts.color {
                    print_diff_key();
                }
                for (i, (name, actual, expected)) in failures.iter().enumerate() {
                    if expected == "NO ERROR" {
                        println!("\n  {}. {name}:\n", i + 1);
                        println!("  Expected an ERROR node, but got:");
                        println!(
                            "  {}",
                            paint(
                                opts.color.then_some(AnsiColor::Red),
                                &format_sexp(actual, 2)
                            )
                        );
                    } else {
                        println!("\n  {}. {name}:", i + 1);
                        let actual = format_sexp(actual, 2);
                        let expected = format_sexp(expected, 2);
                        print_diff(&actual, &expected, opts.color);
                    }
                }
            }

            if has_parse_errors {
                Err(anyhow!(indoc! {"
                    Some tests failed to parse with unexpected `ERROR` or `MISSING` nodes, as shown above, and cannot be updated automatically.
                    Either fix the grammar or manually update the tests if this is expected."}))
            } else {
                Err(anyhow!(""))
            }
        }
    }
}

#[allow(clippy::type_complexity)]
pub fn get_test_info<'test>(
    test_entry: &'test TestEntry,
    target_test: u32,
    test_num: &mut u32,
) -> Option<(&'test str, &'test [u8], Vec<Box<str>>)> {
    match test_entry {
        TestEntry::Example {
            name,
            input,
            attributes,
            ..
        } => {
            if *test_num == target_test {
                return Some((name, input, attributes.languages.clone()));
            }
            *test_num += 1;
        }
        TestEntry::Group { children, .. } => {
            for child in children {
                if let Some((name, input, languages)) = get_test_info(child, target_test, test_num)
                {
                    return Some((name, input, languages));
                }
            }
        }
    }

    None
}

/// Writes the input of `target_test` to a temporary file and returns the path
pub fn get_tmp_test_file(target_test: u32, color: bool) -> Result<(PathBuf, Vec<Box<str>>)> {
    let current_dir = std::env::current_dir().unwrap();
    let test_dir = current_dir.join("test").join("corpus");

    // Get the input of the target test
    let test_entry = parse_tests(&test_dir)?;
    let mut test_num = 0;
    let Some((test_name, test_contents, languages)) =
        get_test_info(&test_entry, target_test - 1, &mut test_num)
    else {
        return Err(anyhow!("Failed to fetch contents of test #{target_test}"));
    };

    // Write the test contents to a temporary file
    let test_path = std::env::temp_dir().join(".tree-sitter-test");
    let mut test_file = std::fs::File::create(&test_path)?;
    test_file.write_all(test_contents)?;

    println!(
        "{target_test}. {}\n",
        paint(color.then_some(AnsiColor::Green), test_name)
    );

    Ok((test_path, languages))
}

pub fn check_queries_at_path(language: &Language, path: &Path) -> Result<()> {
    if path.exists() {
        for entry in WalkDir::new(path)
            .into_iter()
            .filter_map(std::result::Result::ok)
            .filter(|e| {
                e.file_type().is_file()
                    && e.path().extension().and_then(OsStr::to_str) == Some("scm")
                    && !e.path().starts_with(".")
            })
        {
            let filepath = entry.file_name().to_str().unwrap_or("");
            let content = fs::read_to_string(entry.path())
                .with_context(|| format!("Error reading query file {filepath:?}"))?;
            Query::new(language, &content)
                .with_context(|| format!("Error in query file {filepath:?}"))?;
        }
    }
    Ok(())
}

pub fn print_diff_key() {
    println!(
        "\ncorrect / {} / {}",
        paint(Some(AnsiColor::Green), "expected"),
        paint(Some(AnsiColor::Red), "unexpected")
    );
}

pub fn print_diff(actual: &str, expected: &str, use_color: bool) {
    let diff = TextDiff::from_lines(actual, expected);
    for diff in diff.iter_all_changes() {
        match diff.tag() {
            ChangeTag::Equal => {
                if use_color {
                    print!("{diff}");
                } else {
                    print!(" {diff}");
                }
            }
            ChangeTag::Insert => {
                if use_color {
                    print!("{}", paint(Some(AnsiColor::Green), diff.as_str().unwrap()));
                } else {
                    print!("+{diff}");
                }
                if diff.missing_newline() {
                    println!();
                }
            }
            ChangeTag::Delete => {
                if use_color {
                    print!("{}", paint(Some(AnsiColor::Red), diff.as_str().unwrap()));
                } else {
                    print!("-{diff}");
                }
                if diff.missing_newline() {
                    println!();
                }
            }
        }
    }

    println!();
}

pub fn paint(color: Option<impl Into<Color>>, text: &str) -> String {
    let style = Style::new().fg_color(color.map(Into::into));
    format!("{style}{text}{style:#}")
}

/// This will return false if we want to "fail fast". It will bail and not parse any more tests.
#[allow(clippy::too_many_arguments)]
fn run_tests(
    parser: &mut Parser,
    test_entry: TestEntry,
    opts: &mut TestOptions,
    mut indent_level: u32,
    failures: &mut Vec<(String, String, String)>,
    corrected_entries: &mut Vec<(String, String, String, String, usize, usize)>,
    has_parse_errors: &mut bool,
) -> Result<bool> {
    match test_entry {
        TestEntry::Example {
            name,
            input,
            output,
            header_delim_len,
            divider_delim_len,
            has_fields,
            attributes_str,
            attributes,
        } => {
            print!("{}", "  ".repeat(indent_level as usize));

            if attributes.skip {
                println!(
                    "{:>3}. ⌀ {}",
                    opts.test_num,
                    paint(opts.color.then_some(AnsiColor::Yellow), &name),
                );
                opts.test_num += 1;
                return Ok(true);
            }

            if !attributes.platform {
                println!(
                    "{:>3}. ⌀ {}",
                    opts.test_num,
                    paint(opts.color.then_some(AnsiColor::Magenta), &name),
                );
                opts.test_num += 1;
                return Ok(true);
            }

            for (i, language_name) in attributes.languages.iter().enumerate() {
                if !language_name.is_empty() {
                    let language = opts
                        .languages
                        .get(language_name.as_ref())
                        .ok_or_else(|| anyhow!("Language not found: {language_name}"))?;
                    parser.set_language(language)?;
                }
                let tree = parser.parse(&input, None).unwrap();

                if attributes.error {
                    if tree.root_node().has_error() {
                        println!(
                            "{:>3}. ✓ {}",
                            opts.test_num,
                            paint(opts.color.then_some(AnsiColor::Green), &name)
                        );
                        if opts.update {
                            let input = String::from_utf8(input.clone()).unwrap();
                            let output = format_sexp(&output, 0);
                            corrected_entries.push((
                                name.clone(),
                                input,
                                output,
                                attributes_str.clone(),
                                header_delim_len,
                                divider_delim_len,
                            ));
                        }
                    } else {
                        if opts.update {
                            let input = String::from_utf8(input.clone()).unwrap();
                            // Keep the original `expected` output if the actual output has no error
                            let output = format_sexp(&output, 0);
                            corrected_entries.push((
                                name.clone(),
                                input,
                                output,
                                attributes_str.clone(),
                                header_delim_len,
                                divider_delim_len,
                            ));
                        }
                        println!(
                            "{:>3}. ✗ {}",
                            opts.test_num,
                            paint(opts.color.then_some(AnsiColor::Red), &name)
                        );
                        failures.push((
                            name.clone(),
                            tree.root_node().to_sexp(),
                            "NO ERROR".to_string(),
                        ));
                    }

                    if attributes.fail_fast {
                        return Ok(false);
                    }
                } else {
                    let mut actual = tree.root_node().to_sexp();
                    if !(opts.show_fields || has_fields) {
                        actual = strip_sexp_fields(&actual);
                    }

                    if actual == output {
                        println!(
                            "{:>3}. ✓ {}",
                            opts.test_num,
                            paint(opts.color.then_some(AnsiColor::Green), &name)
                        );
                        if opts.update {
                            let input = String::from_utf8(input.clone()).unwrap();
                            let output = format_sexp(&output, 0);
                            corrected_entries.push((
                                name.clone(),
                                input,
                                output,
                                attributes_str.clone(),
                                header_delim_len,
                                divider_delim_len,
                            ));
                        }
                    } else {
                        if opts.update {
                            let input = String::from_utf8(input.clone()).unwrap();
                            let expected_output = format_sexp(&output, 0);
                            let actual_output = format_sexp(&actual, 0);

                            // Only bail early before updating if the actual is not the output,
                            // sometimes users want to test cases that
                            // are intended to have errors, hence why this
                            // check isn't shown above
                            if actual.contains("ERROR") || actual.contains("MISSING") {
                                *has_parse_errors = true;

                                // keep the original `expected` output if the actual output has an
                                // error
                                corrected_entries.push((
                                    name.clone(),
                                    input,
                                    expected_output,
                                    attributes_str.clone(),
                                    header_delim_len,
                                    divider_delim_len,
                                ));
                            } else {
                                corrected_entries.push((
                                    name.clone(),
                                    input,
                                    actual_output,
                                    attributes_str.clone(),
                                    header_delim_len,
                                    divider_delim_len,
                                ));
                                println!(
                                    "{:>3}. ✓ {}",
                                    opts.test_num,
                                    paint(opts.color.then_some(AnsiColor::Blue), &name),
                                );
                            }
                        } else {
                            println!(
                                "{:>3}. ✗ {}",
                                opts.test_num,
                                paint(opts.color.then_some(AnsiColor::Red), &name),
                            );
                        }
                        failures.push((name.clone(), actual, output.clone()));

                        if attributes.fail_fast {
                            return Ok(false);
                        }
                    }
                }

                if i == attributes.languages.len() - 1 {
                    // reset to the first language
                    parser.set_language(opts.languages.values().next().unwrap())?;
                }
            }
            opts.test_num += 1;
        }
        TestEntry::Group {
            name,
            children,
            file_path,
        } => {
            if children.is_empty() {
                return Ok(true);
            }

            indent_level += 1;
            let mut advance_counter = opts.test_num;
            let failure_count = failures.len();
            let mut has_printed = false;

            let matches_filter = |name: &str, opts: &TestOptions| {
                if let Some(include) = &opts.include {
                    include.is_match(name)
                } else if let Some(exclude) = &opts.exclude {
                    !exclude.is_match(name)
                } else {
                    true
                }
            };

            let mut should_skip = |entry: &TestEntry, opts: &TestOptions| match entry {
                TestEntry::Example { name, .. } => {
                    advance_counter += 1;
                    !matches_filter(name, opts)
                }
                TestEntry::Group { .. } => {
                    advance_counter += count_subtests(entry);
                    false
                }
            };

            for child in children {
                if let TestEntry::Example {
                    ref name,
                    ref input,
                    ref output,
                    ref attributes_str,
                    header_delim_len,
                    divider_delim_len,
                    ..
                } = child
                {
                    if should_skip(&child, opts) {
                        let input = String::from_utf8(input.clone()).unwrap();
                        let output = format_sexp(output, 0);
                        corrected_entries.push((
                            name.clone(),
                            input,
                            output,
                            attributes_str.clone(),
                            header_delim_len,
                            divider_delim_len,
                        ));

                        opts.test_num += 1;

                        continue;
                    }
                }
                if !has_printed && indent_level > 1 {
                    has_printed = true;
                    print!("{}", "  ".repeat((indent_level - 1) as usize));
                    println!("{name}:");
                }
                if !run_tests(
                    parser,
                    child,
                    opts,
                    indent_level,
                    failures,
                    corrected_entries,
                    has_parse_errors,
                )? {
                    // fail fast
                    return Ok(false);
                }
            }

            if let Some(file_path) = file_path {
                if opts.update && failures.len() - failure_count > 0 {
                    write_tests(&file_path, corrected_entries)?;
                }
                corrected_entries.clear();
            }
        }
    }
    Ok(true)
}

fn count_subtests(test_entry: &TestEntry) -> usize {
    match test_entry {
        TestEntry::Example { .. } => 1,
        TestEntry::Group { children, .. } => children
            .iter()
            .fold(0, |count, child| count + count_subtests(child)),
    }
}

fn write_tests(
    file_path: &Path,
    corrected_entries: &[(String, String, String, String, usize, usize)],
) -> Result<()> {
    let mut buffer = fs::File::create(file_path)?;
    write_tests_to_buffer(&mut buffer, corrected_entries)
}

fn write_tests_to_buffer(
    buffer: &mut impl Write,
    corrected_entries: &[(String, String, String, String, usize, usize)],
) -> Result<()> {
    for (i, (name, input, output, attributes_str, header_delim_len, divider_delim_len)) in
        corrected_entries.iter().enumerate()
    {
        if i > 0 {
            writeln!(buffer)?;
        }
        writeln!(
            buffer,
            "{}\n{name}\n{}{}\n{input}\n{}\n\n{}",
            "=".repeat(*header_delim_len),
            if attributes_str.is_empty() {
                attributes_str.clone()
            } else {
                format!("{attributes_str}\n")
            },
            "=".repeat(*header_delim_len),
            "-".repeat(*divider_delim_len),
            output.trim()
        )?;
    }
    Ok(())
}

pub fn parse_tests(path: &Path) -> io::Result<TestEntry> {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    if path.is_dir() {
        let mut children = Vec::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let hidden = entry.file_name().to_str().unwrap_or("").starts_with('.');
            if !hidden {
                children.push(entry.path());
            }
        }
        children.sort_by(|a, b| {
            a.file_name()
                .unwrap_or_default()
                .cmp(b.file_name().unwrap_or_default())
        });
        let children = children
            .iter()
            .map(|path| parse_tests(path))
            .collect::<io::Result<Vec<TestEntry>>>()?;
        Ok(TestEntry::Group {
            name,
            children,
            file_path: None,
        })
    } else {
        let content = fs::read_to_string(path)?;
        Ok(parse_test_content(name, &content, Some(path.to_path_buf())))
    }
}

#[must_use]
pub fn strip_sexp_fields(sexp: &str) -> String {
    SEXP_FIELD_REGEX.replace_all(sexp, " (").to_string()
}

#[must_use]
pub fn strip_points(sexp: &str) -> String {
    POINT_REGEX.replace_all(sexp, "").to_string()
}

fn parse_test_content(name: String, content: &str, file_path: Option<PathBuf>) -> TestEntry {
    let mut children = Vec::new();
    let bytes = content.as_bytes();
    let mut prev_name = String::new();
    let mut prev_attributes_str = String::new();
    let mut prev_header_end = 0;

    // Find the first test header in the file, and determine if it has a
    // custom suffix. If so, then this suffix will be used to identify
    // all subsequent headers and divider lines in the file.
    let first_suffix = HEADER_REGEX
        .captures(bytes)
        .and_then(|c| c.name("suffix1"))
        .map(|m| String::from_utf8_lossy(m.as_bytes()));

    // Find all of the `===` test headers, which contain the test names.
    // Ignore any matches whose suffix does not match the first header
    // suffix in the file.
    let header_matches = HEADER_REGEX.captures_iter(bytes).filter_map(|c| {
        let header_delim_len = c.name("equals").map_or(80, |m| m.as_bytes().len());
        let suffix1 = c
            .name("suffix1")
            .map(|m| String::from_utf8_lossy(m.as_bytes()));
        let suffix2 = c
            .name("suffix2")
            .map(|m| String::from_utf8_lossy(m.as_bytes()));

        let (mut skip, mut platform, mut fail_fast, mut error, mut languages) =
            (false, None, false, false, vec![]);

        let test_name_and_markers = c
            .name("test_name_and_markers")
            .map_or("".as_bytes(), |m| m.as_bytes());

        let mut test_name = String::new();
        let mut attributes_str = String::new();

        let mut seen_marker = false;

        let test_name_and_markers = str::from_utf8(test_name_and_markers).unwrap();
        for line in test_name_and_markers
            .split_inclusive('\n')
            .filter(|s| !s.is_empty())
        {
            let trimmed_line = line.trim();
            match trimmed_line.split('(').next().unwrap() {
                ":skip" => (seen_marker, skip) = (true, true),
                ":platform" => {
                    if let Some(platforms) = trimmed_line.strip_prefix(':').and_then(|s| {
                        s.strip_prefix("platform(")
                            .and_then(|s| s.strip_suffix(')'))
                    }) {
                        seen_marker = true;
                        platform = Some(
                            platform.unwrap_or(false) || platforms.trim() == std::env::consts::OS,
                        );
                    }
                }
                ":fail-fast" => (seen_marker, fail_fast) = (true, true),
                ":error" => (seen_marker, error) = (true, true),
                ":language" => {
                    if let Some(lang) = trimmed_line.strip_prefix(':').and_then(|s| {
                        s.strip_prefix("language(")
                            .and_then(|s| s.strip_suffix(')'))
                    }) {
                        seen_marker = true;
                        languages.push(lang.into());
                    }
                }
                _ if !seen_marker => {
                    test_name.push_str(line);
                }
                _ => {}
            }
        }
        attributes_str.push_str(test_name_and_markers.strip_prefix(&test_name).unwrap());

        // prefer skip over error, both shouldn't be set
        if skip {
            error = false;
        }

        // add a default language if none are specified, will defer to the first language
        if languages.is_empty() {
            languages.push("".into());
        }

        if suffix1 == first_suffix && suffix2 == first_suffix {
            let header_range = c.get(0).unwrap().range();
            let test_name = if test_name.is_empty() {
                None
            } else {
                Some(test_name.trim_end().to_string())
            };
            let attributes_str = if attributes_str.is_empty() {
                None
            } else {
                Some(attributes_str.trim_end().to_string())
            };
            Some((
                header_delim_len,
                header_range,
                test_name,
                attributes_str,
                TestAttributes {
                    skip,
                    platform: platform.unwrap_or(true),
                    fail_fast,
                    error,
                    languages,
                },
            ))
        } else {
            None
        }
    });

    let (mut prev_header_len, mut prev_attributes) = (80, TestAttributes::default());
    for (header_delim_len, header_range, test_name, attributes_str, attributes) in header_matches
        .chain(Some((
            80,
            bytes.len()..bytes.len(),
            None,
            None,
            TestAttributes::default(),
        )))
    {
        // Find the longest line of dashes following each test description. That line
        // separates the input from the expected output. Ignore any matches whose suffix
        // does not match the first suffix in the file.
        if prev_header_end > 0 {
            let divider_range = DIVIDER_REGEX
                .captures_iter(&bytes[prev_header_end..header_range.start])
                .filter_map(|m| {
                    let divider_delim_len = m.name("hyphens").map_or(80, |m| m.as_bytes().len());
                    let suffix = m
                        .name("suffix")
                        .map(|m| String::from_utf8_lossy(m.as_bytes()));
                    if suffix == first_suffix {
                        let range = m.get(0).unwrap().range();
                        Some((
                            divider_delim_len,
                            (prev_header_end + range.start)..(prev_header_end + range.end),
                        ))
                    } else {
                        None
                    }
                })
                .max_by_key(|(_, range)| range.len());

            if let Some((divider_delim_len, divider_range)) = divider_range {
                if let Ok(output) = str::from_utf8(&bytes[divider_range.end..header_range.start]) {
                    let mut input = bytes[prev_header_end..divider_range.start].to_vec();

                    // Remove trailing newline from the input.
                    input.pop();
                    if input.last() == Some(&b'\r') {
                        input.pop();
                    }

                    // Remove all comments
                    let output = COMMENT_REGEX.replace_all(output, "").to_string();

                    // Normalize the whitespace in the expected output.
                    let output = WHITESPACE_REGEX.replace_all(output.trim(), " ");
                    let output = output.replace(" )", ")");

                    // Identify if the expected output has fields indicated. If not, then
                    // fields will not be checked.
                    let has_fields = SEXP_FIELD_REGEX.is_match(&output);

                    let t = TestEntry::Example {
                        name: prev_name,
                        input,
                        output,
                        header_delim_len: prev_header_len,
                        divider_delim_len,
                        has_fields,
                        attributes_str: prev_attributes_str,
                        attributes: prev_attributes,
                    };

                    children.push(t);
                }
            }
        }
        prev_attributes = attributes;
        prev_name = test_name.unwrap_or_default();
        prev_attributes_str = attributes_str.unwrap_or_default();
        prev_header_len = header_delim_len;
        prev_header_end = header_range.end;
    }
    TestEntry::Group {
        name,
        children,
        file_path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_test_content_simple() {
        let entry = parse_test_content(
            "the-filename".to_string(),
            r"
===============
The first test
===============

a b c

---

(a
    (b c))

================
The second test
================
d
---
(d)
        "
            .trim(),
            None,
        );

        assert_eq!(
            entry,
            TestEntry::Group {
                name: "the-filename".to_string(),
                children: vec![
                    TestEntry::Example {
                        name: "The first test".to_string(),
                        input: b"\na b c\n".to_vec(),
                        output: "(a (b c))".to_string(),
                        header_delim_len: 15,
                        divider_delim_len: 3,
                        has_fields: false,
                        attributes_str: String::new(),
                        attributes: TestAttributes::default(),
                    },
                    TestEntry::Example {
                        name: "The second test".to_string(),
                        input: b"d".to_vec(),
                        output: "(d)".to_string(),
                        header_delim_len: 16,
                        divider_delim_len: 3,
                        has_fields: false,
                        attributes_str: String::new(),
                        attributes: TestAttributes::default(),
                    },
                ],
                file_path: None,
            }
        );
    }

    #[test]
    fn test_parse_test_content_with_dashes_in_source_code() {
        let entry = parse_test_content(
            "the-filename".to_string(),
            r"
==================
Code with dashes
==================
abc
---
defg
----
hijkl
-------

(a (b))

=========================
Code ending with dashes
=========================
abc
-----------
-------------------

(c (d))
        "
            .trim(),
            None,
        );

        assert_eq!(
            entry,
            TestEntry::Group {
                name: "the-filename".to_string(),
                children: vec![
                    TestEntry::Example {
                        name: "Code with dashes".to_string(),
                        input: b"abc\n---\ndefg\n----\nhijkl".to_vec(),
                        output: "(a (b))".to_string(),
                        header_delim_len: 18,
                        divider_delim_len: 7,
                        has_fields: false,
                        attributes_str: String::new(),
                        attributes: TestAttributes::default(),
                    },
                    TestEntry::Example {
                        name: "Code ending with dashes".to_string(),
                        input: b"abc\n-----------".to_vec(),
                        output: "(c (d))".to_string(),
                        header_delim_len: 25,
                        divider_delim_len: 19,
                        has_fields: false,
                        attributes_str: String::new(),
                        attributes: TestAttributes::default(),
                    },
                ],
                file_path: None,
            }
        );
    }

    #[test]
    fn test_format_sexp() {
        assert_eq!(format_sexp("", 0), "");
        assert_eq!(
            format_sexp("(a b: (c) (d) e: (f (g (h (MISSING i)))))", 0),
            r"
(a
  b: (c)
  (d)
  e: (f
    (g
      (h
        (MISSING i)))))
"
            .trim()
        );
        assert_eq!(
            format_sexp("(program (ERROR (UNEXPECTED ' ')) (identifier))", 0),
            r"
(program
  (ERROR
    (UNEXPECTED ' '))
  (identifier))
"
            .trim()
        );
        assert_eq!(
            format_sexp(r#"(source_file (MISSING ")"))"#, 0),
            r#"
(source_file
  (MISSING ")"))
        "#
            .trim()
        );
        assert_eq!(
            format_sexp(
                r"(source_file (ERROR (UNEXPECTED 'f') (UNEXPECTED '+')))",
                0
            ),
            r"
(source_file
  (ERROR
    (UNEXPECTED 'f')
    (UNEXPECTED '+')))
"
            .trim()
        );
    }

    #[test]
    fn test_write_tests_to_buffer() {
        let mut buffer = Vec::new();
        let corrected_entries = vec![
            (
                "title 1".to_string(),
                "input 1".to_string(),
                "output 1".to_string(),
                String::new(),
                80,
                80,
            ),
            (
                "title 2".to_string(),
                "input 2".to_string(),
                "output 2".to_string(),
                String::new(),
                80,
                80,
            ),
        ];
        write_tests_to_buffer(&mut buffer, &corrected_entries).unwrap();
        assert_eq!(
            String::from_utf8(buffer).unwrap(),
            r"
================================================================================
title 1
================================================================================
input 1
--------------------------------------------------------------------------------

output 1

================================================================================
title 2
================================================================================
input 2
--------------------------------------------------------------------------------

output 2
"
            .trim_start()
            .to_string()
        );
    }

    #[test]
    fn test_parse_test_content_with_comments_in_sexp() {
        let entry = parse_test_content(
            "the-filename".to_string(),
            r#"
==================
sexp with comment
==================
code
---

; Line start comment
(a (b))

==================
sexp with comment between
==================
code
---

; Line start comment
(a
; ignore this
    (b)
    ; also ignore this
)

=========================
sexp with ';'
=========================
code
---

(MISSING ";")
        "#
            .trim(),
            None,
        );

        assert_eq!(
            entry,
            TestEntry::Group {
                name: "the-filename".to_string(),
                children: vec![
                    TestEntry::Example {
                        name: "sexp with comment".to_string(),
                        input: b"code".to_vec(),
                        output: "(a (b))".to_string(),
                        header_delim_len: 18,
                        divider_delim_len: 3,
                        has_fields: false,
                        attributes_str: String::new(),
                        attributes: TestAttributes::default(),
                    },
                    TestEntry::Example {
                        name: "sexp with comment between".to_string(),
                        input: b"code".to_vec(),
                        output: "(a (b))".to_string(),
                        header_delim_len: 18,
                        divider_delim_len: 3,
                        has_fields: false,
                        attributes_str: String::new(),
                        attributes: TestAttributes::default(),
                    },
                    TestEntry::Example {
                        name: "sexp with ';'".to_string(),
                        input: b"code".to_vec(),
                        output: "(MISSING \";\")".to_string(),
                        header_delim_len: 25,
                        divider_delim_len: 3,
                        has_fields: false,
                        attributes_str: String::new(),
                        attributes: TestAttributes::default(),
                    }
                ],
                file_path: None,
            }
        );
    }

    #[test]
    fn test_parse_test_content_with_suffixes() {
        let entry = parse_test_content(
            "the-filename".to_string(),
            r"
==================asdf\()[]|{}*+?^$.-
First test
==================asdf\()[]|{}*+?^$.-

=========================
NOT A TEST HEADER
=========================
-------------------------

---asdf\()[]|{}*+?^$.-

(a)

==================asdf\()[]|{}*+?^$.-
Second test
==================asdf\()[]|{}*+?^$.-

=========================
NOT A TEST HEADER
=========================
-------------------------

---asdf\()[]|{}*+?^$.-

(a)

=========================asdf\()[]|{}*+?^$.-
Test name with = symbol
=========================asdf\()[]|{}*+?^$.-

=========================
NOT A TEST HEADER
=========================
-------------------------

---asdf\()[]|{}*+?^$.-

(a)

==============================asdf\()[]|{}*+?^$.-
Test containing equals
==============================asdf\()[]|{}*+?^$.-

===

------------------------------asdf\()[]|{}*+?^$.-

(a)

==============================asdf\()[]|{}*+?^$.-
Subsequent test containing equals
==============================asdf\()[]|{}*+?^$.-

===

------------------------------asdf\()[]|{}*+?^$.-

(a)
"
            .trim(),
            None,
        );

        let expected_input = b"\n=========================\n\
            NOT A TEST HEADER\n\
            =========================\n\
            -------------------------\n"
            .to_vec();
        pretty_assertions::assert_eq!(
            entry,
            TestEntry::Group {
                name: "the-filename".to_string(),
                children: vec![
                    TestEntry::Example {
                        name: "First test".to_string(),
                        input: expected_input.clone(),
                        output: "(a)".to_string(),
                        header_delim_len: 18,
                        divider_delim_len: 3,
                        has_fields: false,
                        attributes_str: String::new(),
                        attributes: TestAttributes::default(),
                    },
                    TestEntry::Example {
                        name: "Second test".to_string(),
                        input: expected_input.clone(),
                        output: "(a)".to_string(),
                        header_delim_len: 18,
                        divider_delim_len: 3,
                        has_fields: false,
                        attributes_str: String::new(),
                        attributes: TestAttributes::default(),
                    },
                    TestEntry::Example {
                        name: "Test name with = symbol".to_string(),
                        input: expected_input,
                        output: "(a)".to_string(),
                        header_delim_len: 25,
                        divider_delim_len: 3,
                        has_fields: false,
                        attributes_str: String::new(),
                        attributes: TestAttributes::default(),
                    },
                    TestEntry::Example {
                        name: "Test containing equals".to_string(),
                        input: "\n===\n".into(),
                        output: "(a)".into(),
                        header_delim_len: 30,
                        divider_delim_len: 30,
                        has_fields: false,
                        attributes_str: String::new(),
                        attributes: TestAttributes::default(),
                    },
                    TestEntry::Example {
                        name: "Subsequent test containing equals".to_string(),
                        input: "\n===\n".into(),
                        output: "(a)".into(),
                        header_delim_len: 30,
                        divider_delim_len: 30,
                        has_fields: false,
                        attributes_str: String::new(),
                        attributes: TestAttributes::default(),
                    }
                ],
                file_path: None,
            }
        );
    }

    #[test]
    fn test_parse_test_content_with_newlines_in_test_names() {
        let entry = parse_test_content(
            "the-filename".to_string(),
            r"
===============
name
with
newlines
===============
a
---
(b)

====================
name with === signs
====================
code with ----
---
(d)
",
            None,
        );

        assert_eq!(
            entry,
            TestEntry::Group {
                name: "the-filename".to_string(),
                file_path: None,
                children: vec![
                    TestEntry::Example {
                        name: "name\nwith\nnewlines".to_string(),
                        input: b"a".to_vec(),
                        output: "(b)".to_string(),
                        header_delim_len: 15,
                        divider_delim_len: 3,
                        has_fields: false,
                        attributes_str: String::new(),
                        attributes: TestAttributes::default(),
                    },
                    TestEntry::Example {
                        name: "name with === signs".to_string(),
                        input: b"code with ----".to_vec(),
                        output: "(d)".to_string(),
                        header_delim_len: 20,
                        divider_delim_len: 3,
                        has_fields: false,
                        attributes_str: String::new(),
                        attributes: TestAttributes::default(),
                    }
                ]
            }
        );
    }

    #[test]
    fn test_parse_test_with_markers() {
        // do one with :skip, we should not see it in the entry output

        let entry = parse_test_content(
            "the-filename".to_string(),
            r"
=====================
Test with skip marker
:skip
=====================
a
---
(b)
",
            None,
        );

        assert_eq!(
            entry,
            TestEntry::Group {
                name: "the-filename".to_string(),
                file_path: None,
                children: vec![TestEntry::Example {
                    name: "Test with skip marker".to_string(),
                    input: b"a".to_vec(),
                    output: "(b)".to_string(),
                    header_delim_len: 21,
                    divider_delim_len: 3,
                    has_fields: false,
                    attributes_str: ":skip".to_string(),
                    attributes: TestAttributes {
                        skip: true,
                        platform: true,
                        fail_fast: false,
                        error: false,
                        languages: vec!["".into()]
                    },
                }]
            }
        );

        let entry = parse_test_content(
            "the-filename".to_string(),
            &format!(
                r"
=========================
Test with platform marker
:platform({})
:fail-fast
=========================
a
---
(b)

=============================
Test with bad platform marker
:platform({})

:language(foo)
=============================
a
---
(b)
",
                std::env::consts::OS,
                if std::env::consts::OS == "linux" {
                    "macos"
                } else {
                    "linux"
                }
            ),
            None,
        );

        assert_eq!(
            entry,
            TestEntry::Group {
                name: "the-filename".to_string(),
                file_path: None,
                children: vec![
                    TestEntry::Example {
                        name: "Test with platform marker".to_string(),
                        input: b"a".to_vec(),
                        output: "(b)".to_string(),
                        header_delim_len: 25,
                        divider_delim_len: 3,
                        has_fields: false,
                        attributes_str: format!(":platform({})\n:fail-fast", std::env::consts::OS),
                        attributes: TestAttributes {
                            skip: false,
                            platform: true,
                            fail_fast: true,
                            error: false,
                            languages: vec!["".into()]
                        },
                    },
                    TestEntry::Example {
                        name: "Test with bad platform marker".to_string(),
                        input: b"a".to_vec(),
                        output: "(b)".to_string(),
                        header_delim_len: 29,
                        divider_delim_len: 3,
                        has_fields: false,
                        attributes_str: if std::env::consts::OS == "linux" {
                            ":platform(macos)\n\n:language(foo)".to_string()
                        } else {
                            ":platform(linux)\n\n:language(foo)".to_string()
                        },
                        attributes: TestAttributes {
                            skip: false,
                            platform: false,
                            fail_fast: false,
                            error: false,
                            languages: vec!["foo".into()]
                        },
                    }
                ]
            }
        );
    }
}
