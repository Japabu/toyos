# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

### New Features

 - <csr-id-461c87667c75a9db0a74c43ef68d71b88a7dd754/> Add an `auto-chain-error` feature to let `gix-error::Error` produce error chains suitable for `anyhow`.
 - <csr-id-28f4211afadd91c5b5d2d2a0698f37e660cc0c66/> make it possible to produce errors that work well with `anyhow` source-chain display.
 - <csr-id-053c3ee2217480eead3aa7c71fa4b65455444921/> anyhow support for `gix-error::Exn`
   This is mainly useful for `gitoxide-core`, which may call plumbing.
 - <csr-id-3301eb8b2906861952726061629d74babfd24f73/> Add `Exn::downcast_any_ref()`

### New Features (BREAKING)

 - <csr-id-5ab19a7a3344c58ad1185a23a789848ed5e02241/> Use `gix-error` in `gix-date`
   This will make for easier introspection for users of these errors.

### Refactor (BREAKING)

 - <csr-id-829393ac596bf2684bd8a837ae931773b24ee033/> ErrorExt::raise_iter to raise_all + remove Frame::downcast
   Be more compatible to `exn`.
 - <csr-id-f8517bedcbb9b3328f435aa37f4c63bd30b19fc0/> catch up Exn designs with the upstream
   refactor!: rename `Exn::from_iter` to `raise_all`

### Commit Statistics

<csr-read-only-do-not-edit/>

 - 25 commits contributed to the release over the course of 12 calendar days.
 - 7 commits were understood as [conventional](https://www.conventionalcommits.org).
 - 2 unique issues were worked on: [#2384](https://github.com/GitoxideLabs/gitoxide/issues/2384), [#2385](https://github.com/GitoxideLabs/gitoxide/issues/2385)

### Commit Details

<csr-read-only-do-not-edit/>

<details><summary>view details</summary>

 * **[#2384](https://github.com/GitoxideLabs/gitoxide/issues/2384)**
    - Catch up Exn designs with the upstream ([`f8517be`](https://github.com/GitoxideLabs/gitoxide/commit/f8517bedcbb9b3328f435aa37f4c63bd30b19fc0))
 * **[#2385](https://github.com/GitoxideLabs/gitoxide/issues/2385)**
    - ErrorExt::raise_iter to raise_all + remove Frame::downcast ([`829393a`](https://github.com/GitoxideLabs/gitoxide/commit/829393ac596bf2684bd8a837ae931773b24ee033))
 * **Uncategorized**
    - Merge pull request #2383 from GitoxideLabs/gix-error ([`9d39656`](https://github.com/GitoxideLabs/gitoxide/commit/9d39656710c297f9a22e4a7e6facc3a1f35f89e0))
    - Address Copilot review ([`16327ef`](https://github.com/GitoxideLabs/gitoxide/commit/16327efe24e2321e2a4efe5321e9f0483484b10a))
    - Add an `auto-chain-error` feature to let `gix-error::Error` produce error chains suitable for `anyhow`. ([`461c876`](https://github.com/GitoxideLabs/gitoxide/commit/461c87667c75a9db0a74c43ef68d71b88a7dd754))
    - Make it possible to produce errors that work well with `anyhow` source-chain display. ([`28f4211`](https://github.com/GitoxideLabs/gitoxide/commit/28f4211afadd91c5b5d2d2a0698f37e660cc0c66))
    - Anyhow support for `gix-error::Exn` ([`053c3ee`](https://github.com/GitoxideLabs/gitoxide/commit/053c3ee2217480eead3aa7c71fa4b65455444921))
    - Merge pull request #2378 from GitoxideLabs/gix-error ([`6cff657`](https://github.com/GitoxideLabs/gitoxide/commit/6cff65786b5213194fffd2c77b7c2dc44dcb4b52))
    - Address Copilot review ([`e112cac`](https://github.com/GitoxideLabs/gitoxide/commit/e112cacc42a192d5159b299e49739f3af2589e3e))
    - Change `ErrorExt::erased()` to `ErrorExt::raise_erased()`. ([`373fced`](https://github.com/GitoxideLabs/gitoxide/commit/373fceddcc1a0ef79f306b519a2ca3682b3110ef))
    - Make `Exn` work properly after the type was erased. ([`499402c`](https://github.com/GitoxideLabs/gitoxide/commit/499402c941e85e6cff5c3ffef8a09afac842c7ac))
    - Add `Exn::downcast_any_ref()` ([`3301eb8`](https://github.com/GitoxideLabs/gitoxide/commit/3301eb8b2906861952726061629d74babfd24f73))
    - Merge pull request #2374 from GitoxideLabs/gix-error ([`25233ce`](https://github.com/GitoxideLabs/gitoxide/commit/25233ced7f17e14842aa400cf007a0feb6127d89))
    - Turn `Exn::into_box()` to `Exn::into_inner()`. ([`939b8fc`](https://github.com/GitoxideLabs/gitoxide/commit/939b8fcbb2115eba77aca1be8527ad0d7f644c56))
    - Merge pull request #2373 from GitoxideLabs/gix-error ([`4c6a7a7`](https://github.com/GitoxideLabs/gitoxide/commit/4c6a7a76c214c94910f141542d677dc2a7500ddd))
    - Adapt to changes in `gix-chunk` ([`e6e90ff`](https://github.com/GitoxideLabs/gitoxide/commit/e6e90ff82b1f839a6d78170685f2a69566766675))
    - Add conversion from Message to `ParseError` for less noisy invocations. ([`08f9ed4`](https://github.com/GitoxideLabs/gitoxide/commit/08f9ed48c896ea92d8d8da9b15bc44f7709b013e))
    - More docs to better explain `gix-error` ([`f46ca99`](https://github.com/GitoxideLabs/gitoxide/commit/f46ca9925930e4b2a660d2896ccbfb8edd3aa4e9))
    - Merge pull request #2352 from GitoxideLabs/gix-error ([`ae23762`](https://github.com/GitoxideLabs/gitoxide/commit/ae23762932ea0d78e91463185a304d778746a167))
    - Make it possible to traverse frames using an iterator. ([`3bac149`](https://github.com/GitoxideLabs/gitoxide/commit/3bac149385a6f64ab0ee1989ad562132574dc021))
    - Actually introduce `gix-error` into `gix-revision`. ([`4819ea8`](https://github.com/GitoxideLabs/gitoxide/commit/4819ea8d81645b8b79dc2a3fcba7b27d773a9fce))
    - Adadpt `exn` to most pressing needs of `gitoxide` ([`abedade`](https://github.com/GitoxideLabs/gitoxide/commit/abedadec5463b57e78aa53e62d8c511b989ae9ca))
    - Vendor `exn` from https://github.com/fast/exn@bb4d8ea4e4df335c46d4fa3f4f260121f9f84305 ([`0eaab70`](https://github.com/GitoxideLabs/gitoxide/commit/0eaab70ee6e897635d7fb41402ec87387b8ecd4b))
    - Use `gix-error` in `gix-date` ([`5ab19a7`](https://github.com/GitoxideLabs/gitoxide/commit/5ab19a7a3344c58ad1185a23a789848ed5e02241))
    - Create a basic `gix-error` crate to forward `exn` ([`35cf1ff`](https://github.com/GitoxideLabs/gitoxide/commit/35cf1ff837ea30a1366b20bde0d59baf9ab699be))
</details>

