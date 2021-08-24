use anyhow::{anyhow, bail, Result};
use colored::{ColoredString, Colorize};
use std::path::Path;
use url::Url;
use git_commands::git;
use std::process::Command;
use std::collections::HashMap;

mod trim;
use trim::TrimAsciiWhitespace;

macro_rules! regex {
    ($re:literal $(,)?) => {{
        static RE: once_cell::sync::OnceCell<regex::Regex> = once_cell::sync::OnceCell::new();
        RE.get_or_init(|| regex::Regex::new($re).unwrap())
    }};
}

fn main() -> Result<()> {
    // Get the list of git branches and the diff numbers for them.
    let working_dir = std::env::current_dir()?;

    let arc_info = get_arc_info(&working_dir)?;

    let branches = get_branches(&working_dir)?;

    let max_branch_len = branches.iter().map(|branch| branch.branch.len()).max().unwrap_or_default();

    for branch in branches {
        let diffs = get_branch_diffs(&working_dir, &branch.branch, "master")?;

        print!(
            "{:width$}",
            if diffs.is_empty() { branch.branch.normal() } else { branch.branch.bold() },
            width = max_branch_len + 2,
        );

        for diff in diffs {
            print!(" {}", diff.bold());
            if let Some(info) = arc_info.get(&diff) {
                print!(" ({})", coloured_status(&info.status));
            }
        }

        println!();
    }
    Ok(())
}

// Get the output of `arc list`.
fn arc_list(working_dir: &Path) -> Result<Vec<String>> {
    let output = Command::new("arc")
        .args(&["list"])
        .current_dir(working_dir)
        .output()?;

    if !output.status.success() {
        bail!("arc list command failed: {:?}", output);
    }

    let output = std::str::from_utf8(output.stdout.trim_ascii_whitespace())?;

    Ok(output.lines().map(|line| line.to_owned()).collect())
}

#[derive(Debug)]
struct ArcInfo {
    // Not sure what this means.
    exists: bool,
    //   'Closed'          => 'cyan',
    //   'Needs Review'    => 'magenta',
    //   'Needs Revision'  => 'red',
    //   'Changes Planned' => 'red',
    //   'Accepted'        => 'green',
    //   'No Revision'     => 'blue',
    //   'Abandoned'       => 'default',
    status: String,
    summary: String,
}

fn coloured_status(status: &str) -> ColoredString {
    match status {
        "Closed"          => status.cyan(),
        "Needs Review"    => status.magenta(),
        "Needs Revision"  => status.red(),
        "Changes Planned" => status.red(),
        "Accepted"        => status.green(),
        "No Revision"     => status.blue(),
        "Abandoned"       => status.dimmed(),
        _                 => status.normal(),
    }
}

fn get_arc_info(working_dir: &Path) -> Result<HashMap<String, ArcInfo>> {
    // Output of `arc list` is:

    // 1. "You have no open Differential revisions." if you have no open diffs.
    // 2. A table with the columns:
    //     * Exists (an asterisk or blank)
    //     * Status ("Needs Review" etc)
    //     * Title ("D1234: Foo bar")
    //
    // Unfortunately the table is not fixed width - it depends on the content.
    // Easy solution is a regex.

    let lines = arc_list(working_dir)?;

    if lines == &["You have no open Differential revisions."] {
        return Ok(HashMap::new());
    }

    lines.iter().map(|line| {
        let re = regex!(r#"^(?P<exists>\* )?(?P<status>[\w ]+) (?P<diff>D\d+): (?P<summary>.*)$"#);
        let caps = re.captures(line).ok_or_else(|| anyhow!("Couldn't parse line: {:?}", line))?;

        let exists = caps.name("exists").is_some();
        let status = caps["status"].trim().to_owned();
        let diff = caps["diff"].to_owned();
        let summary = caps["summary"].trim().to_owned();

        Ok((
            diff,
            ArcInfo{
                exists,
                status,
                summary,
            },
        ))
    }).collect::<Result<_, _>>()
}



#[derive(Debug)]
struct BranchInfo {
    branch: String,
    upstream: Option<String>,
}

fn get_branches(working_dir: &Path) -> Result<Vec<BranchInfo>> {
    use std::str;

    // TODO: Config system to allow specifying the branches? Maybe allow adding/removing them?
    // Store config in `.git/autorebase/autorebase.toml` or `autorebase.toml`?

    let output = git(
        &[
            "for-each-ref",
            "--format=%(refname:short)%00%(upstream:short)",
            "refs/heads",
        ],
        working_dir,
    )?
    .stdout;

    let branches = output
        .split(|c| *c == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            let parts: Vec<&[u8]> = line.split(|c| *c == 0).collect();
            if parts.len() != 2 {
                bail!(
                    "for-each-ref parse error, got {} parts, expected 3",
                    parts.len()
                );
            }

            let branch = str::from_utf8(parts[0])?.to_owned();

            let upstream = if parts[1].is_empty() {
                None
            } else {
                Some(str::from_utf8(parts[1])?.to_owned())
            };

            Ok(BranchInfo {
                branch,
                upstream,
            })
        })
        .collect::<Result<_, _>>()?;

    Ok(branches)
}

fn get_branch_diffs(working_dir: &Path, branch: &str, target_branch: &str) -> Result<Vec<String>> {
    let merge_base = get_merge_base(working_dir, branch, target_branch)?;

    let bodies = get_commit_bodies(working_dir, &merge_base, branch)?;

    Ok(bodies.iter().rev().filter_map(|s| get_differential_revision(s)).collect())
}

fn get_merge_base(working_dir: &Path, a: &str, b: &str) -> Result<String> {
    let output = git(&["merge-base", a, b], working_dir)?.stdout;
    let output = std::str::from_utf8(output.trim_ascii_whitespace())?;
    Ok(output.to_owned())
}

/// Get the bodies of the commits from `from` to `to`. This includes `to` but not
/// `from`. Each Vec element is one line from a body but the bodies are all
/// joined together.
fn get_commit_bodies(working_dir: &Path, from: &str, to: &str) -> Result<Vec<String>> {
    let output = git(
        &[
            "--no-pager",
            "log",
            "--format=%B",
            &format!("{}..{}", from, to),
        ],
        working_dir,
    )?
    .stdout;
    let output = String::from_utf8(output)?;
    Ok(output.lines().map(ToOwned::to_owned).collect())
}

/// If the line is of the form "Differential revision: http(s)://.../D1234" then
/// return Some("D1234").
fn get_differential_revision(line: &str) -> Option<String> {
    if let Some(url_str) = line.strip_prefix("Differential Revision:") {
        if let Ok(url) = Url::parse(url_str.trim()) {
            let path = url.path();
            if let Some(diff_number) = path.strip_prefix("/D") {
                if diff_number.chars().all(|c| c.is_ascii_digit()) {
                    return Some(path[1..].to_owned());
                }
            }
        }
    }
    None
}
