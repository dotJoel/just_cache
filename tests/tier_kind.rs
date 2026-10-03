//! The tier seam, through the parser a real config goes through.
//!
//! §2 says a tier's `kind` is "which transport driver serves this tier". `fs`, `object`
//! and `peer` have drivers today; `offline` does not, so what these tests pin is the
//! refusal for the latter: a kind nothing serves must not be accepted and then served as
//! an ordinary local directory, which is what a free-text `kind` did until this seam
//! existed (invariant 3 — refuse what you do not understand). The two facts the seam has
//! to answer are pinned here too: which kinds have a driver, and which ones put bytes on
//! the far side of the machine boundary (rule 2, the envelope).

use std::path::Path;

use just_cache::tiers::{TierKind, TierSet, TiersError};

fn config(kind: &str) -> String {
    format!(
        "[tiers.tier]\nkind = \"{kind}\"\npath = \"/mnt/tier\"\nvolatility = \"persistent\"\n\
         recall = \"ms\"\ncopies = 1\n"
    )
}

fn config_object(kind: &str) -> String {
    format!(
        "[tiers.tier]\nkind = \"{kind}\"\npath = \"/mnt/tier\"\nvolatility = \"persistent\"\n\
         recall = \"min\"\ncopies = 1\nendpoint = \"s3.example.com\"\nbucket = \"my-bucket\"\n\
         region = \"us-east-1\"\ncredential_source = \"$S3_KEY\"\n\
         encryption_key = \"$ENC_KEY\"\n"
    )
}

/// A `peer` tier's config: the same S3-compatible fields as `object`, without a region
/// (a peer has no meaningful region — the object-server derives it from the request's
/// credential scope, so the driver defaults it to `peer`).
fn config_peer(extra: &str) -> String {
    format!(
        "[tiers.tier]\nkind = \"peer\"\npath = \"/mnt/tier\"\nvolatility = \"persistent\"\n\
         recall = \"min\"\ncopies = 1\nendpoint = \"192.168.0.50:9000\"\nbucket = \"my-bucket\"\n\
         credential_source = \"$PEER_KEY\"\nencryption_key = \"$ENC_KEY\"\n{extra}\n"
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
        if kind == TierKind::Fs || kind == TierKind::Object || kind == TierKind::Peer {
            assert!(kind.is_served(), "{kind} is served by a driver");
            if kind == TierKind::Fs {
                assert!(
                    !kind.crosses_machine_boundary(),
                    "a local root keeps its bytes on this machine, so rule 2 does not apply"
                );
            } else {
                assert!(
                    kind.crosses_machine_boundary(),
                    "`{kind}` leaves this machine — the envelope is required"
                );
            }
        } else {
            assert!(
                !kind.is_served(),
                "no driver serves `{kind}` yet; is_served is what a driver flips"
            );
            assert!(
                kind.crosses_machine_boundary(),
                "`{kind}` leaves this machine — bytes (peer) or the volume itself \
                 (offline) — so the envelope is required"
            );
        }
    }
}

/// A kind the design names but no driver serves is refused, naming the tier, the kind and
/// the line — the input that used to parse and then behave as a local directory.
#[test]
fn a_kind_with_no_driver_is_refused_by_name() {
    let error = TierSet::parse(&config("offline"), Path::new("/tmp/tiers.toml"))
        .expect_err("an unserved kind must be refused");
    let text = error.to_string();
    assert!(
        text.contains("kind `offline` has no transport driver yet"),
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

/// An `object` tier now has a driver: it parses with its required fields, and a missing
/// field is refused by line rather than passing through silently.
#[test]
fn an_object_tier_parses_with_its_required_fields() {
    let set = TierSet::parse(&config_object("object"), Path::new("/tmp/tiers.toml")).unwrap();
    let tier = set.get("tier").unwrap();
    assert_eq!(tier.kind, TierKind::Object);
    assert!(tier.object_config.is_some());
    let obj = tier.object_config.as_ref().unwrap();
    assert_eq!(obj.endpoint, "s3.example.com");
    assert_eq!(obj.bucket, "my-bucket");
    assert_eq!(obj.region, "us-east-1");
    assert_eq!(
        obj.remote_kind,
        just_cache::object_store::RemoteKind::S3,
        "an object tier drives the S3 remote"
    );
}

/// A `peer` tier now has a driver: it parses with the same S3-compatible fields as an
/// `object` tier, and `region` defaults to `peer` (a peer has no meaningful region).
#[test]
fn a_peer_tier_parses_with_its_required_fields() {
    let set = TierSet::parse(&config_peer(""), Path::new("/tmp/tiers.toml")).unwrap();
    let tier = set.get("tier").unwrap();
    assert_eq!(tier.kind, TierKind::Peer);
    assert!(tier.object_config.is_some());
    let obj = tier.object_config.as_ref().unwrap();
    assert_eq!(obj.endpoint, "192.168.0.50:9000");
    assert_eq!(obj.bucket, "my-bucket");
    assert_eq!(
        obj.region, "peer",
        "a peer tier defaults its signing region to `peer`"
    );
    assert_eq!(
        obj.remote_kind,
        just_cache::object_store::RemoteKind::Peer,
        "a peer tier drives the peer remote"
    );
}

/// A `peer` tier honours an explicit region when the operator writes one.
#[test]
fn a_peer_tier_accepts_an_explicit_region() {
    let set = TierSet::parse(
        &config_peer("region = \"lan\""),
        Path::new("/tmp/tiers.toml"),
    )
    .unwrap();
    let obj = set.get("tier").unwrap().object_config.as_ref().unwrap();
    assert_eq!(obj.region, "lan");
}

/// A `peer` tier missing a required field is refused with the field name and line, the
/// same way an `object` tier is.
#[test]
fn a_peer_tier_without_endpoint_is_refused_by_line() {
    let text = concat!(
        "[tiers.tier]\nkind = \"peer\"\npath = \"/mnt/tier\"\nvolatility = \"persistent\"\n",
        "recall = \"min\"\ncopies = 1\nbucket = \"b\"\n",
        "credential_source = \"$X\"\nencryption_key = \"$ENC_KEY\"\n"
    );
    let err = TierSet::parse(text, Path::new("/tmp/tiers.toml")).unwrap_err();
    assert!(err.to_string().contains("endpoint"), "{err}");
    assert!(err.to_string().contains("line 2"), "{err}");
}

/// An object tier missing a required field is refused with the field name and line.
#[test]
fn an_object_tier_without_endpoint_is_refused_by_line() {
    let text = concat!(
        "[tiers.tier]\nkind = \"object\"\npath = \"/mnt/tier\"\nvolatility = \"persistent\"\n",
        "recall = \"min\"\ncopies = 1\nbucket = \"b\"\nregion = \"us\"\n",
        "credential_source = \"$X\"\nencryption_key = \"$ENC_KEY\"\n"
    );
    let err = TierSet::parse(text, Path::new("/tmp/tiers.toml")).unwrap_err();
    assert!(err.to_string().contains("endpoint"), "{err}");
    assert!(err.to_string().contains("line 2"), "{err}");
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
