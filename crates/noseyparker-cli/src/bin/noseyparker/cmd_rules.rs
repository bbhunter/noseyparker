use anyhow::{bail, Context, Result};
use regex::Regex;
use std::collections::HashSet;
use vectorscan::{BlockDatabase, Flag, Pattern, Scan};

use tracing::{debug_span, error, error_span, info, warn};

use crate::args;
use noseyparker::rules::{Rule, Rules};
use noseyparker::rules_database::RulesDatabase;

pub fn run(global_args: &args::GlobalArgs, args: &args::RulesArgs) -> Result<()> {
    match &args.command {
        args::RulesCommand::Check(args) => cmd_rules_check(global_args, args),
    }
}

fn cmd_rules_check(_global_args: &args::GlobalArgs, args: &args::RulesCheckArgs) -> Result<()> {
    let _span = debug_span!("cmd_rules_check").entered();

    let rules = Rules::from_paths(&args.inputs).context("Failed to load input rules")?;
    let mut num_errors = 0;
    let mut num_warnings = 0;
    let num_rules = rules.rules.len();

    // ensure IDs are globally unique
    {
        let mut seen_ids = HashSet::<&str>::new();
        for rule in rules.rules.iter() {
            let rule_id = &rule.id;
            if !seen_ids.insert(rule_id) {
                error!("Rule ID {rule_id} is not unique");
                num_errors += 1;
            }
        }
    }

    // ensure IDs are well-formed
    {
        let id_pat = Regex::new(r"^[a-zA-Z0-9]+(?:[.-][a-zA-Z0-9]+)*$")
            .expect("ID validator pattern should compile");

        for rule in rules.rules.iter() {
            let rule_id = &rule.id;
            const ID_LIMIT: usize = 20;
            let rule_id_len = rule_id.len();
            if rule_id_len > ID_LIMIT {
                error!("Rule ID {rule_id} is too long ({rule_id_len} characters: \
                       should be {ID_LIMIT} characters max)");
                num_errors += 1;
            }

            if !id_pat.is_match(rule_id) {
                error!("Rule ID {rule_id} is not well-formed: \
                       it should consist only of alphanumeric sections \
                       delimited by hyphens or periods");
                num_errors += 1;
            }
        }
    }

    // compile the rules individually
    for (rule_num, rule) in rules.rules.iter().enumerate() {
        let stats = check_rule(rule_num, rule)?;
        num_errors += stats.num_errors;
        num_warnings += stats.num_warnings;
    }

    // compile the rules all together
    let _rules_db =
        RulesDatabase::from_rules(rules).context("Failed to compile combined rules database")?;

    if num_warnings == 0 && num_errors == 0 {
        println!("{num_rules} rules: no issues detected");
    } else {
        println!("{num_rules} rules: {num_errors} errors and {num_warnings} warnings");
    }

    if num_errors != 0 {
        bail!("{num_errors} errors in rules");
    }

    if num_warnings != 0 && args.warnings_as_errors {
        bail!("{num_warnings} warnings; warnings being treated as errors");
    }

    Ok(())
}

fn hs_compile_pattern(pat: &str) -> Result<BlockDatabase> {
    let pat = pat.as_bytes().to_vec();
    let db = BlockDatabase::new(vec![Pattern::new(pat, Flag::default(), None)])?;
    Ok(db)
}

// fn hs_compile_pattern_streaming(pat: &str) -> Result<StreamingDatabase> {
//     let pattern = pattern!{pat};
//     let mut pattern = pattern.left_most();
//     pattern.som = Some(vectorscan::SomHorizon::Large);
//     let db: StreamingDatabase = pattern.build()?;
//     Ok(db)
// }

struct CheckStats {
    num_warnings: usize,
    num_errors: usize,
}

fn check_rule(rule_num: usize, rule: &Rule) -> Result<CheckStats> {
    let _span = error_span!("rule", "{}:{}", rule_num + 1, rule.name).entered();

    let mut num_warnings = 0;
    let mut num_errors = 0;

    let num_examples = rule.examples.len();
    if num_examples == 0 {
        warn!("Rule has no examples");
        num_warnings += 1;
    }

    match rule.as_regex() {
        Err(e) => {
            error!("Regex: failed to compile pattern: {e}");
            num_errors += 1;
        }
        Ok(pat) => {
            let mut num_succeeded = 0;
            let mut num_failed = 0;

            // Check positive examples
            for (example_num, example) in rule.examples.iter().enumerate() {
                if pat.find(example.as_bytes()).is_none() {
                    error!("Regex: failed to match example {example_num}");
                    num_failed += 1;
                    num_errors += 1;
                } else {
                    num_succeeded += 1;
                }
            }

            // Check negative examples
            for (example_num, example) in rule.negative_examples.iter().enumerate() {
                if pat.find(example.as_bytes()).is_some() {
                    error!("Regex: incorrectly matched negative example {example_num}");
                    num_failed += 1;
                    num_errors += 1;
                } else {
                    num_succeeded += 1;
                }
            }

            let num_total = num_succeeded + num_failed;
            if num_total > 0 {
                info!("Regex: {num_succeeded}/{num_total} examples succeeded");
            }
        }
    };

    // match hs_compile_pattern_streaming(&rule.pattern) {
    //     Err(e) => {
    //         error!("Vectorscan: failed to compile streaming pattern: {}", e);
    //         num_errors += 1;
    //     }
    //     Ok(_db) => {}
    // }

    match hs_compile_pattern(&rule.uncommented_pattern()) {
        Err(e) => {
            error!("Vectorscan: failed to compile pattern: {e}");
            num_errors += 1;
        }
        Ok(db) => {
            let mut scanner = vectorscan::BlockScanner::new(&db)?;

            let mut num_succeeded = 0;
            let mut num_failed = 0;

            // Check positive examples
            for (example_num, example) in rule.examples.iter().enumerate() {
                let mut matched = false;
                scanner.scan(example.as_bytes(), |_id, _from, _to, _flags| {
                    matched = true;
                    Scan::Continue
                })?;
                if !matched {
                    error!("Vectorscan: failed to match example {example_num}");
                    num_failed += 1;
                    num_errors += 1;
                } else {
                    num_succeeded += 1;
                }
            }

            // Check negative examples
            for (example_num, example) in rule.negative_examples.iter().enumerate() {
                let mut matched = false;
                scanner.scan(example.as_bytes(), |_id, _from, _to, _flags| {
                    matched = true;
                    Scan::Continue
                })?;
                if matched {
                    error!("Vectorscan: incorrectly matched negative example {example_num}");
                    num_failed += 1;
                    num_errors += 1;
                } else {
                    num_succeeded += 1;
                }
            }

            let num_total = num_succeeded + num_failed;
            if num_total > 0 {
                info!("Vectorscan: {num_succeeded}/{num_total} examples succeeded");
            }
        }
    }

    if num_warnings == 0 && num_errors == 0 {
        info!("No issues detected");
    } else {
        info!("{num_errors} errors and {num_warnings} warnings");
    }

    Ok(CheckStats {
        num_warnings,
        num_errors,
    })
}
