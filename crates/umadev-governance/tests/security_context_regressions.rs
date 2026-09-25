use umadev_governance::{
    check_hardcoded_secret, check_plaintext_password, check_sql_injection,
    check_unhashed_password_storage,
};

#[test]
fn entropy_fallback_allows_stable_code_owned_literals_on_shipping_paths() {
    let cases = [
        (
            "crates/umadev-knowledge/src/repomap.rs",
            r###"let rust_fn = r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+(?:\x22[^\x22]*\x22\s+)?)?fn\s+([A-Za-z_][\w]*)";"###,
        ),
        (
            "crates/umadev/src/self_update.rs",
            r#"let asset = "umadev-x86_64-apple-darwin";"#,
        ),
        (
            "crates/umadev-tui/src/lib.rs",
            r#"const ABORT_SENTINEL: &str = "\u{2068}umadev-block-aborted\u{2069}";"#,
        ),
    ];

    for (path, source) in cases {
        let decision = check_hardcoded_secret(path, source);
        assert!(
            !decision.block,
            "stable literal in {path} must not trip UD-SEC-003: {}",
            decision.reason
        );
    }
}

#[test]
fn entropy_fallback_still_blocks_an_unnamed_random_credential() {
    let decision = check_hardcoded_secret(
        "src/runtime_config.rs",
        r#"const BLOB: &str = "a1B2c3D4e5F6g7H8i9J0kL3mN9pQ7rS";"#,
    );
    assert!(decision.block, "random credential must still block");
    assert_eq!(decision.clause, "UD-SEC-003");
}

#[test]
fn password_rule_does_not_correlate_unrelated_file_wide_words() {
    let cases = [
        (
            "crates/umadev-agent/src/director_loop.rs",
            [
                "memo.in",
                "sert(red_half_key(root, pre, test), outcome);\nplan_state::save(&plan, root);\nfn login_rejects_bad_pass",
                "word() {}",
            ]
            .concat(),
        ),
        (
            "crates/umadev-contract/src/parse.rs",
            [
                "//! POST /login accepts { email, pass",
                "word }\nif taken.in",
                "sert(operation_id.clone()) { continue; }",
            ]
            .concat(),
        ),
        (
            "crates/umadev-host/src/opencode_session.rs",
            [
                "let pass",
                "word = random_pass",
                "word();\nstatuses.in",
                "sert(id.to_string(), \"busy\".to_string());",
            ]
            .concat(),
        ),
        (
            "crates/umadev-agent/src/critics.rs",
            [
                "OpenOptions::new().cre",
                "ate(true);\nwrite_scratch(root, \"../../etc/pass",
                "wd\", \"x\");",
            ]
            .concat(),
        ),
    ];

    for (path, source) in cases {
        for decision in [
            check_plaintext_password(path, &source),
            check_unhashed_password_storage(path, &source),
        ] {
            assert!(
                !decision.block,
                "unrelated local statements in {path} must pass: {}",
                decision.reason
            );
        }
    }
}

#[test]
fn password_rule_keeps_real_plaintext_storage_and_hash_flow_signal() {
    let plaintext_source = [
        "await db.in",
        "sert({\n  email,\n  pass",
        "word: inputPassword,\n});",
    ]
    .concat();
    let plaintext = check_unhashed_password_storage("server/user.ts", &plaintext_source);
    assert!(plaintext.block, "plaintext insert must still block");
    assert_eq!(plaintext.clause, "UD-SEC-033");

    let adjacent_source = ["user.pass", "word = inputPassword;\nawait user.sa", "ve();"].concat();
    let adjacent = check_unhashed_password_storage("server/user.ts", &adjacent_source);
    assert!(adjacent.block, "plaintext assignment then save must block");

    let hashed = check_unhashed_password_storage(
        "server/user.ts",
        "const encoded = await argon2.hash(inputPassword);\n\
         await db.insert({ email, password: encoded });",
    );
    assert!(!hashed.block, "a locally proven hash flow must pass");
}

#[test]
fn dynamic_sql_injection_signal_is_unchanged() {
    let decision = check_sql_injection(
        "server/users.ts",
        "const query = \"SELECT * FROM users WHERE id = \" + userId; db.query(query);",
    );
    assert!(decision.block, "dynamic SQL must still block");
    assert_eq!(decision.clause, "UD-SEC-011");
}

#[test]
fn regression_fixture_is_not_itself_a_plaintext_finding() {
    for decision in [
        check_plaintext_password(
            "crates/umadev-governance/tests/security_context_regressions.rs",
            include_str!("security_context_regressions.rs"),
        ),
        check_unhashed_password_storage(
            "crates/umadev-governance/tests/security_context_regressions.rs",
            include_str!("security_context_regressions.rs"),
        ),
    ] {
        assert!(
            !decision.block,
            "the executable regression fixture must scan clean: {}",
            decision.reason
        );
    }
}
