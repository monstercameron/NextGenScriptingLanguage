//! Seeded adversarial checks: malformed programs never execute native commands.
use flow::{manifest::Registry, parser, schedule::CronSchedule, stream, ui};
use std::path::Path;

fn random(state: &mut u64) -> usize {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state as usize
}

fn mutate(source: &str, state: &mut u64) -> String {
    let alphabet = [
        'a', '0', ' ', '\n', '\r', '\t', '"', '\\', '[', ']', ',', '#', ':', '%', '\0', '\x1b',
        'é', '界', '🦀', '\u{a0}',
    ];
    let mut chars: Vec<char> = source.chars().collect();
    for _ in 0..(1 + random(state) % 10) {
        let index = random(state) % (chars.len() + 1);
        let value = alphabet[random(state) % alphabet.len()];
        match random(state) % 3 {
            0 if index < chars.len() => {
                chars.remove(index);
            }
            1 if index < chars.len() => chars[index] = value,
            _ => chars.insert(index, value),
        }
    }
    chars.into_iter().collect()
}

#[test]
fn seeded_program_mutations_never_panic_in_any_parser() {
    let seeds = [
        "run printf with message \"hi\" as result\nthen cat\n",
        "read \"input.txt\"\nthen tr with from \"a-z\" and to \"A-Z\" and cache\nthen save to \"result.txt\"\n",
        "listen to stdin\nwhenever a line arrives as record\n  then cat\n",
        "every 5 milliseconds\n  run pwd\n",
        "on schedule \"*/5 * * * *\"\n  run pwd\n",
        "stream from cat with files [\"one\", \"two\"]\npipe to cat\ntake the first 3 lines\nshow each line\n",
        include_str!("../examples/live/widget-console.program"),
        include_str!("../examples/top-cpu.program"),
    ];
    let mut state = 0x35d2_497a_21a9_87bdu64;
    for trial in 0..4096 {
        let source = mutate(seeds[random(&mut state) % seeds.len()], &mut state);
        let checked = std::panic::catch_unwind(|| {
            let first = parser::parse(&source);
            let second = parser::parse(&source);
            assert_eq!(
                first.is_ok(),
                second.is_ok(),
                "nondeterministic finite parse"
            );
            if let (Ok(first), Ok(second)) = (first, second) {
                assert_eq!(first, second);
            }
            let _ = stream::parse(&source);
            let _ = ui::syntax::parse(&source);
        });
        assert!(checked.is_ok(), "parser panic at trial {trial}: {source:?}");
    }
}

#[test]
fn seeded_strings_keep_exact_argv_boundaries_and_reject_nul() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests/printf.json");
    let registry = Registry::load(&manifest).unwrap();
    let manifest = registry.resolve("printf").unwrap();
    let mut state = 0x9138_9a17_56b3_94cdu64;
    for _ in 0..1024 {
        let value = mutate(
            "$(touch never-created); `printf unsafe` | * $HOME é界🦀\n",
            &mut state,
        );
        let source = format!(
            "run printf with message {}\n",
            serde_json::to_string(&value).unwrap()
        );
        let program = parser::parse(&source).unwrap();
        let flow::ast::StepKind::Invoke { call, .. } = &program.steps[0].kind else {
            unreachable!()
        };
        if value.contains('\0') {
            assert!(manifest.validate(call).is_err());
            assert!(manifest.build_arguments(&call.parameters).is_err());
        } else {
            manifest.validate(call).unwrap();
            assert_eq!(
                manifest.build_arguments(&call.parameters).unwrap(),
                [r"%s\n".to_owned(), value]
            );
        }
    }
}

#[test]
fn seeded_manifest_mutations_fail_without_panicking() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("manifest.json");
    let seed = r#"{"name":"fixture","binary":"printf","parameters":{"message":{"type":"string","required":true}},"arguments":[{"literal":"%s"},{"parameter":"message"}]}"#;
    let mut state = 0xb37e_f9aa_2438_4d19u64;
    for trial in 0..1024 {
        let source = mutate(seed, &mut state);
        std::fs::write(&path, &source).unwrap();
        assert!(
            std::panic::catch_unwind(|| Registry::load(&path)).is_ok(),
            "manifest panic at trial {trial}: {source:?}"
        );
    }
}

#[test]
fn seeded_cron_mutations_and_extreme_steps_never_panic() {
    let seeds = [
        "* * * * *",
        "*/5 0-23/2 1,15 * 0,7",
        "0 9 * * 1-5",
        "0 0 */4294967295 * *",
        "4294967295 * * * *",
    ];
    let mut state = 0x2948_b19c_35a8_f0e7u64;
    for trial in 0..4096 {
        let source = mutate(seeds[random(&mut state) % seeds.len()], &mut state);
        assert!(
            std::panic::catch_unwind(|| CronSchedule::parse(&source)).is_ok(),
            "cron panic at trial {trial}: {source:?}"
        );
    }
}
