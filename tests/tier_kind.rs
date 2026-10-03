//! The tier seam, through the parser a real config goes through.
//!
//! §2 says a tier's `kind` is "which transport driver serves this tier". Only `fs` has a
//! driver today, so what these tests pin is the refusal: a kind nothing serves must not be
//! accepted and then served as an ordinary local directory, which is what a free-text
//! `kind` did until this seam existed (invariant 3 — refuse what you do not understand).
//! The two facts the seam has to answer are pinned here too: which kinds have a driver, and
//! which ones put bytes on the far side of the machine boundary (rule 2, the envelope).

use std::path::Path;

use just_cache::tiers::{TierKind, TierSet, TiersError};

fn config(kind: &str) -> String {
    format!(
        "[tiers.tier]\nkind = \"{kind}\"\npath = \"/mnt/tier\"\nvolatility = \"persistent\"\n\
         recall = \"ms\"\ncopies = 1\n"
    )
}

/// The type answers the two questions a driver seam exists to answer, without a config.
#[test]
fn the_kind_always_answers_the_seam_questions() {
    assert_eq!(
        TierKind::names(),
        "fs, object, peer, offline",
        "a refusal lists the set the design names"
    );

    for kind in TierKind::ALL {
        if kind == TierKind::Fs {
            assert!(kind.is_served(), "fs is the one kind with a driver");
            assert!(
                !kind.crosses_machine_boundary(),
                "a local root keeps its bytes on this machine, so rule 2 does not apply"
            );
        } else {
            assert!(
                !kind.is_served(),
                "no driver serves `{kind}` yet; is_served is what a driver flips"
            );
            assert!(
                kind.crosses_machine_boundary(),
                "`{kind}` leaves this machine — bytes (object, peer) or the volume itself \
                 (offline) — so the envelope is required"
            );
        }
    }
}

/// A kind the design names but no driver serves is refused, naming the tier, the kind and
/// the line — the input that used to parse and then behave as a local directory.
#[test]
fn a_kind_with_no_driver_is_refused_by_name() {
    for kind in ["object", "peer", "offline"] {
        let error = TierSet::parse(&config(kind), Path::new("/tmp/tiers.toml"))
            .expect_err("an unserved kind must be refused");
        let text = error.to_string();
        assert!(
            text.contains(&format!("kind `{kind}` has no transport driver yet")),
            "the refusal must say which kind and why: {text}"
        );
        assert!(
            text.contains("tier `tier`") && text.contains("line 2"),
            "the refusal must name the tier and the line: {text}"
        );
        assert!(
            matches!(error, TiersError::Invalid { .. }),
            "an unserved kind is an invalid config, not a syntax error"
        );
    }
}

/// A kind nothing recognises is refused with the set the design names, so a typo is answered
/// with the valid spellings rather than being treated as a filesystem.
#[test]
fn an_unknown_kind_is_refused_with_the_supported_set() {
    let text = TierSet::parse(&config("s3"), Path::new("/tmp/tiers.toml"))
        .expect_err("an unknown kind must be refused")
        .to_string();
    assert!(
        text.contains("is not one of fs, object, peer, offline"),
        "the refusal must list the set: {text}"
    );
    assert!(text.contains("line 2"), "and where: {text}");
}

/// An empty `kind` keeps its own message rather than being reported as an unknown spelling:
/// the empty value is a different mistake (the field was written, with nothing in it).
#[test]
fn an_empty_kind_still_names_its_line() {
    let text = TierSet::parse(&config(""), Path::new("/tmp/tiers.toml"))
        .expect_err("an empty kind must be refused")
        .to_string();
    assert!(text.contains("empty `kind`"), "{text}");
    assert!(text.contains("line 2"), "{text}");
}
