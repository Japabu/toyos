use std::borrow::Cow;

use gix_ref::{Category, FullName, FullNameRef, PartialNameRef};

#[test]
fn cow() {
    fn _fn(_x: Cow<'_, FullNameRef>) {}
    fn _pn(_x: Cow<'_, PartialNameRef>) {}
}

#[test]
fn file_name() {
    let name: gix_ref::FullName = "refs/heads/main".try_into().unwrap();
    assert_eq!(name.as_ref().file_name(), "main");
}
#[test]
fn shorten_and_category() {
    for (input, expected, category, is_worktree_private) in [
        ("refs/tags/tag-name", "tag-name", Category::Tag, false),
        ("refs/heads/main", "main", Category::LocalBranch, false),
        ("refs/remotes/origin/main", "origin/main", Category::RemoteBranch, false),
        ("refs/notes/note-name", "notes/note-name", Category::Note, false),
        ("HEAD", "HEAD", Category::PseudoRef, true),
        ("FETCH_HEAD", "FETCH_HEAD", Category::PseudoRef, true),
        ("main-worktree/HEAD", "HEAD", Category::MainPseudoRef, true),
        ("main-worktree/FETCH_HEAD", "FETCH_HEAD", Category::MainPseudoRef, true),
        (
            "main-worktree/refs/heads/main",
            "refs/heads/main",
            Category::MainRef,
            false,
        ),
        (
            "main-worktree/refs/notes/note",
            "refs/notes/note",
            Category::MainRef,
            false,
        ),
        (
            "worktrees/name/HEAD",
            "HEAD",
            Category::LinkedPseudoRef { name: "name".into() },
            true,
        ),
        (
            "worktrees/name/FETCH_HEAD",
            "FETCH_HEAD",
            Category::LinkedPseudoRef { name: "name".into() },
            true,
        ),
        (
            "worktrees/name/refs/heads/main",
            "refs/heads/main",
            Category::LinkedRef { name: "name".into() },
            false,
        ),
        (
            "worktrees/name/refs/notes/note",
            "refs/notes/note",
            Category::LinkedRef { name: "name".into() },
            false,
        ),
        (
            "worktrees/name/refs/heads/main",
            "refs/heads/main",
            Category::LinkedRef { name: "name".into() },
            false,
        ),
        ("refs/bisect/good", "bisect/good", Category::Bisect, true),
        ("refs/rewritten/123456", "rewritten/123456", Category::Rewritten, true),
        (
            "refs/worktree/private",
            "worktree/private",
            Category::WorktreePrivate,
            true,
        ),
    ] {
        let name: gix_ref::FullName = input.try_into().unwrap();
        assert_eq!(category.is_worktree_private(), is_worktree_private);
        let category = Some(category);
        assert_eq!(name.as_ref().shorten(), expected);
        assert_eq!(name.shorten(), expected);
        assert_eq!(name.category(), category);

        let cat_and_short_name = name.category_and_short_name();
        match category {
            None => {
                assert_eq!(cat_and_short_name, None);
            }
            Some(expected_category) => {
                assert_eq!(cat_and_short_name, Some((expected_category, expected.into())));
                let (cat, short_name) = cat_and_short_name.expect("we know it's set");
                let actual = cat.to_full_name(short_name).expect("valid input = valid output");
                assert_eq!(
                    actual.as_ref().as_bstr(),
                    input,
                    "{input}: {cat:?}:{short_name}: categories and short-names can round-trip"
                );
            }
        }
        assert_eq!(name.as_ref().category(), category);
    }

    for special in ["hello/world", "main-worktree/head"] {
        let name: gix_ref::FullName = special.try_into().unwrap();
        assert_eq!(
            name.shorten(),
            special,
            "the whole name is returned if there is no prefix"
        );
        assert_eq!(name.category(), None);
    }

    assert!(
        Category::LocalBranch.to_full_name("invalid/").is_err(),
        "validation is performed as one would expect"
    );
}

#[test]
fn to_full_name() -> gix_testtools::Result {
    assert_eq!(
        Category::LocalBranch.to_full_name("refs/heads/full")?.as_bstr(),
        "refs/heads/full",
        "prefixes aren't duplicated"
    );

    assert_eq!(
        Category::LocalBranch
            .to_full_name("refs/remotes/origin/other")?
            .as_bstr(),
        "refs/heads/refs/remotes/origin/other",
        "full names with a different category will be prefixed, to support 'main-worktree' special cases"
    );

    Ok(())
}

#[test]
fn prefix_with_namespace_and_stripping() {
    let ns = gix_ref::namespace::expand("foo").unwrap();
    let mut name: gix_ref::FullName = "refs/heads/main".try_into().unwrap();
    assert_eq!(
        name.prefix_namespace(&ns).as_bstr(),
        "refs/namespaces/foo/refs/heads/main"
    );
    assert_eq!(
        name.prefix_namespace(&ns).as_bstr(),
        "refs/namespaces/foo/refs/heads/main",
        "idempotent prefixing"
    );
    assert_eq!(name.strip_namespace(&ns).as_bstr(), "refs/heads/main");
    assert_eq!(
        name.strip_namespace(&ns).as_bstr(),
        "refs/heads/main",
        "idempotent stripping"
    );
}

#[test]
fn display() {
    let full_name = FullName::try_from("refs/heads/main").unwrap();
    assert_eq!(format!("{full_name}"), "refs/heads/main");

    let full_name_ref = full_name.as_ref();
    assert_eq!(format!("{full_name_ref}"), "refs/heads/main");
}
