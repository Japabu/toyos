mod interpolate {
    use std::{
        borrow::Cow,
        path::{Path, PathBuf},
    };

    use gix_config_value::path;

    use crate::{b, cow_str};

    #[test]
    fn backslash_is_not_special_and_they_are_not_escaping_anything() -> crate::Result {
        for path in [r"C:\foo\bar", "/foo/bar"] {
            let actual = gix_config_value::Path::from(Cow::Borrowed(b(path))).interpolate(Default::default())?;
            assert_eq!(actual, Path::new(path));
            assert!(
                matches!(actual, Cow::Borrowed(_)),
                "it does not unnecessarily copy values"
            );
        }
        Ok(())
    }

    #[test]
    fn empty_path_is_error() {
        assert!(matches!(
            interpolate_without_context(""),
            Err(path::interpolate::Error::Missing { what: "path" })
        ));
    }

    #[test]
    fn prefix_substitutes_git_install_dir() {
        for git_install_dir in &["/tmp/git", r"C:\git"] {
            for (val, expected) in &[("%(prefix)/foo/bar", "foo/bar"), (r"%(prefix)/foo\bar", r"foo\bar")] {
                let expected =
                    std::path::PathBuf::from(format!("{}{}{}", git_install_dir, std::path::MAIN_SEPARATOR, expected));
                assert_eq!(
                    gix_config_value::Path::from(cow_str(val))
                        .interpolate(path::interpolate::Context {
                            git_install_dir: Path::new(git_install_dir).into(),
                            ..Default::default()
                        })
                        .unwrap(),
                    expected,
                    "prefix interpolation keeps separators as they are"
                );
            }
        }
    }

    #[test]
    fn prefix_substitution_skipped_with_dot_slash() {
        let path = "./%(prefix)/foo/bar";
        let git_install_dir = "/tmp/git";
        assert_eq!(
            gix_config_value::Path::from(Cow::Borrowed(b(path)))
                .interpolate(path::interpolate::Context {
                    git_install_dir: Path::new(git_install_dir).into(),
                    ..Default::default()
                })
                .unwrap(),
            Path::new(path)
        );
    }

    #[test]
    fn tilde_alone_does_not_interpolate() -> crate::Result {
        assert_eq!(interpolate_without_context("~")?, Path::new("~"));
        Ok(())
    }

    #[test]
    fn tilde_slash_substitutes_current_user() -> crate::Result {
        let path = "~/user/bar";
        let home = std::env::current_dir()?;
        let expected = home.join("user").join("bar");
        assert_eq!(
            gix_config_value::Path::from(cow_str(path))
                .interpolate(path::interpolate::Context {
                    home_dir: Some(&home),
                    home_for_user: Some(home_for_user),
                    ..Default::default()
                })
                .unwrap()
                .as_ref(),
            expected
        );
        Ok(())
    }

    #[cfg(any(target_os = "windows", target_os = "android"))]
    #[test]
    fn tilde_with_given_user_is_unsupported_on_windows_and_android() {
        assert!(matches!(
            interpolate_without_context("~baz/foo/bar"),
            Err(gix_config_value::path::interpolate::Error::UserInterpolationUnsupported)
        ));
    }

    #[cfg(not(any(target_os = "windows", target_os = "android")))]
    #[test]
    fn tilde_with_given_user() -> crate::Result {
        let home = std::env::current_dir()?;

        for path_suffix in &["foo/bar", r"foo\bar", ""] {
            let path = format!("~user{}{}", std::path::MAIN_SEPARATOR, path_suffix);
            let expected = home.join("user").join(path_suffix);

            assert_eq!(interpolate_without_context(path)?, expected);
        }
        Ok(())
    }

    fn interpolate_without_context(
        path: impl AsRef<str>,
    ) -> Result<Cow<'static, Path>, gix_config_value::path::interpolate::Error> {
        gix_config_value::Path::from(Cow::Owned(path.as_ref().to_owned().into())).interpolate(
            path::interpolate::Context {
                home_for_user: Some(home_for_user),
                ..Default::default()
            },
        )
    }

    fn home_for_user(name: &str) -> Option<PathBuf> {
        std::env::current_dir().unwrap().join(name).into()
    }
}

mod optional_prefix {
    use std::borrow::Cow;

    use crate::{b, cow_str};
    use bstr::ByteSlice;

    #[test]
    fn path_without_optional_prefix_is_not_optional() {
        let path = gix_config_value::Path::from(Cow::Borrowed(b("/some/path")));
        assert!(!path.is_optional, "path without prefix should not be optional");
        assert_eq!(path.value.as_ref(), b"/some/path");
    }

    #[test]
    fn path_with_optional_prefix_is_optional() {
        let path = gix_config_value::Path::from(cow_str(":(optional)/some/path"));
        assert!(path.is_optional, "path with :(optional) prefix should be optional");
        assert_eq!(path.value.as_ref(), b"/some/path", "prefix should be stripped");
    }

    #[test]
    fn optional_prefix_with_relative_path() {
        let path = gix_config_value::Path::from(cow_str(":(optional)relative/path"));
        assert!(path.is_optional);
        assert_eq!(path.value.as_ref(), b"relative/path");
    }

    #[test]
    fn optional_prefix_with_tilde_expansion() {
        let path = gix_config_value::Path::from(cow_str(":(optional)~/config/file"));
        assert!(path.is_optional);
        assert_eq!(
            path.value.as_ref(),
            b"~/config/file",
            "tilde should be preserved for interpolation"
        );
    }

    #[test]
    fn optional_prefix_with_prefix_substitution() {
        let path = gix_config_value::Path::from(cow_str(":(optional)%(prefix)/share/git"));
        assert!(path.is_optional);
        assert_eq!(
            path.value.as_ref(),
            b"%(prefix)/share/git",
            "prefix should be preserved for interpolation"
        );
    }

    #[test]
    fn optional_prefix_with_windows_path() {
        let path = gix_config_value::Path::from(cow_str(r":(optional)C:\Users\file"));
        assert!(path.is_optional);
        assert_eq!(path.value.as_ref(), br"C:\Users\file");
    }

    #[test]
    fn optional_prefix_followed_by_empty_path() {
        let path = gix_config_value::Path::from(cow_str(":(optional)"));
        assert!(path.is_optional);
        assert_eq!(path.value.as_ref(), b"", "empty path after prefix is valid");
    }

    #[test]
    fn partial_optional_string_is_not_treated_as_prefix() {
        let path = gix_config_value::Path::from(cow_str(":(opt)ional/path"));
        assert!(
            !path.is_optional,
            "incomplete prefix should not be treated as optional marker"
        );
        assert_eq!(path.value.as_ref(), b":(opt)ional/path");
    }

    #[test]
    fn optional_prefix_case_sensitive() {
        let path = gix_config_value::Path::from(cow_str(":(OPTIONAL)/some/path"));
        assert!(!path.is_optional, "prefix should be case-sensitive");
        assert_eq!(path.value.as_ref(), b":(OPTIONAL)/some/path");
    }

    #[test]
    fn optional_prefix_with_spaces() {
        let path = gix_config_value::Path::from(cow_str(":(optional) /path/with/space"));
        assert!(path.is_optional);
        assert_eq!(
            path.value.as_ref(),
            b" /path/with/space",
            "space after prefix should be preserved"
        );
    }

    #[test]
    fn borrowed_path_stays_borrowed_after_prefix_stripping() {
        // Verify that we don't unnecessarily allocate when stripping the prefix from borrowed data
        let borrowed_input: &[u8] = b":(optional)/some/path";
        let path = gix_config_value::Path::from(Cow::Borrowed(borrowed_input.as_bstr()));

        assert!(path.is_optional);
        assert_eq!(path.value.as_ref(), b"/some/path");
        // Verify it's still borrowed (no unnecessary allocation)
        assert!(matches!(path.value, Cow::Borrowed(_)));
    }

    #[test]
    fn owned_path_stays_owned_after_prefix_stripping() {
        // Verify that owned data remains owned after prefix stripping
        let owned_input = bstr::BString::from(":(optional)/some/path");
        let path = gix_config_value::Path::from(Cow::Owned(owned_input));

        assert!(path.is_optional);
        assert_eq!(path.value.as_ref(), b"/some/path");
        // Verify it's still owned
        assert!(matches!(path.value, Cow::Owned(_)));
    }
}
