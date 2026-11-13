//! Tests of src/users.rs, in a file of their own so code scanning can leave
//! their made-up passwords out (.github/codeql/codeql-config.yml).

use super::*;

#[test]
fn roundtrip_keeps_odd_passwords() {
    let accounts = vec![
        Account {
            name: "ivan".into(),
            password: "p w\tö:x".into(),
        },
        Account {
            name: "sergey".into(),
            password: "#1".into(),
        },
    ];
    assert_eq!(parse(&format(&accounts).unwrap()).unwrap(), accounts);
}

#[test]
fn set_adds_or_changes_ignoring_case() {
    let mut accounts = Vec::new();
    assert!(set(&mut accounts, "Ivan", "a").unwrap());
    assert!(!set(&mut accounts, "ivan", "b").unwrap());
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].name, "Ivan");
    assert_eq!(accounts[0].password, "b");
    assert!(set(&mut accounts, "sergey", "c").unwrap());
    assert_eq!(accounts.len(), 2);
}

#[test]
fn remove_keeps_the_last_account_and_rename_keeps_the_password() {
    let mut accounts = Vec::new();
    set(&mut accounts, "ivan", "a").unwrap();
    set(&mut accounts, "sergey", "b").unwrap();
    assert!(rename(&mut accounts, "Sergey", "ivan").is_err());
    rename(&mut accounts, "Sergey", "serg").unwrap();
    assert_eq!(accounts[1].name, "serg");
    assert_eq!(accounts[1].password, "b");
    assert!(remove(&mut accounts, "nobody").is_err());
    remove(&mut accounts, "IVAN").unwrap();
    assert_eq!(accounts.len(), 1);
    assert!(remove(&mut accounts, "serg").is_err());
}

#[test]
fn rejects_bad_names_and_passwords() {
    let mut accounts = Vec::new();
    for name in ["", "a b", "DOM\\ivan", "ivan@corp", "a\tb"] {
        assert!(set(&mut accounts, name, "x").is_err(), "{name:?}");
    }
    assert!(set(&mut accounts, "ivan", "").is_err());
    assert!(set(&mut accounts, "ivan", "a\nb").is_err());
    assert!(parse("openrdp-users 2\n").is_err());
    assert!(parse("openrdp-users 1\nivan\tx\nIVAN\ty\n").is_err());
}
