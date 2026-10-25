//! Keep the README's public CLI inventory in step with clap and rendered help.
use std::collections::BTreeSet;

use clap::CommandFactory;
use sshtm::cli::Args;

fn code_spans(line: &str) -> impl Iterator<Item = &str> {
    line.split('`').skip(1).step_by(2)
}

fn flags(command: &clap::Command) -> BTreeSet<String> {
    command
        .get_arguments()
        .filter(|arg| !arg.is_hide_set())
        .flat_map(|arg| {
            arg.get_long()
                .map(|name| format!("--{name}"))
                .into_iter()
                .chain(arg.get_short().map(|name| format!("-{name}")))
        })
        .collect()
}

#[test]
fn readme_public_commands_and_flags_match_help() {
    let readme = include_str!("../README.md");
    let inventory = readme
        .split_once("<!-- cli-reference:start -->")
        .unwrap()
        .1
        .split_once("<!-- cli-reference:end -->")
        .unwrap()
        .0;
    let mut root = Args::command();
    root.build();
    let documented: BTreeSet<_> = inventory
        .lines()
        .filter(|line| line.starts_with("| `"))
        .map(|line| code_spans(line).next().unwrap())
        .collect();
    let actual: BTreeSet<_> = root
        .get_subcommands()
        .filter(|cmd| !cmd.is_hide_set())
        .map(|cmd| cmd.get_name())
        .collect();
    assert_eq!(documented, actual);

    let common = inventory
        .lines()
        .find(|line| line.starts_with("Common option:"))
        .unwrap();
    let common_flags: BTreeSet<String> = code_spans(common)
        .filter(|span| span.starts_with('-'))
        .map(str::to_owned)
        .collect();
    assert_eq!(common_flags, flags(&root));
    let help_flags: BTreeSet<String> = common_flags
        .iter()
        .filter(|flag| matches!(flag.as_str(), "-h" | "--help"))
        .cloned()
        .collect();

    for command in std::iter::once(&root).chain(
        root.get_subcommands()
            // clap's generated help command delegates to another command and
            // has no --help flag of its own.
            .filter(|command| !command.is_hide_set() && command.get_name() != "help"),
    ) {
        let mut process = assert_cmd::cargo::cargo_bin_cmd!("sshtm");
        let mut expected_flags = common_flags.clone();
        if command.get_name() != "sshtm" {
            process.arg(command.get_name());
            let line = inventory
                .lines()
                .find(|line| line.starts_with(&format!("| `{}` |", command.get_name())))
                .unwrap();
            expected_flags = help_flags.clone();
            expected_flags.extend(
                code_spans(line)
                    .filter(|span| span.starts_with('-'))
                    .map(str::to_owned),
            );
        }
        assert_eq!(expected_flags, flags(command), "{}", command.get_name());
        let output = process
            .arg("--help")
            .assert()
            .success()
            .get_output()
            .clone();
        let help = String::from_utf8(output.stdout).unwrap();
        for flag in expected_flags {
            assert!(
                help.contains(&flag),
                "{} missing {flag}",
                command.get_name()
            );
        }
        if command.get_name() == "sshtm" {
            for name in &documented {
                assert!(
                    help.lines()
                        .any(|line| line.trim_start().starts_with(&format!("{name} ")))
                );
            }
        } else {
            assert!(
                help.contains("Example"),
                "{} needs examples",
                command.get_name()
            );
        }
    }
    assert_cmd::cargo::cargo_bin_cmd!("sshtm")
        .args(["help", "open"])
        .assert()
        .success();
}
