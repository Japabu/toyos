#!/usr/bin/env bash
set -eu -o pipefail

git init --bare basic
(cd basic
  cat >include-this <<-\EOF
  [user]
    this = this-is-included
EOF

  cat >dont-include-that <<-\EOF
  [user]
    that = that-is-not-included
EOF

  cat >>config <<-EOF
  [includeIf "hasconfig:remote.*.url:foourl"]
    path = "include-this"
  [includeIf "hasconfig:remote.*.url:barurl"]
    path = "dont-include-that"
  [remote "foo"]
    url = foourl
EOF

  git config --get user.this >expected
)
  
git init --bare inclusion-order
(cd inclusion-order
  cat >include-two-three <<-\EOF
  [user]
    two = included-config
    three = included-config
EOF
  cat >include-four <<-\EOF
  [user]
    four = included-config
EOF
  cat >include-five <<-\EOF
  [user]
    five = included-config
EOF
  cat >indirect <<-\EOF
  [includeIf "hasconfig:remote.*.url:early"]
    path = "include-five"
EOF
  cat >>config <<-EOF
  [remote "foo"]
    url = before
  [remote "other"]
    url = early
  [user]
    one = main-config
  [includeIf "hasconfig:remote.*.url:before"]
    path = "include-two-three"
  [includeIf "hasconfig:remote.*.url:after"]
    path = "include-four"
  [user]
    three = main-config
    five = main-config
  [remote "bar"]
    url = after
  [include]
    path = "indirect"
EOF
  git config --get user.one >expected.one
  git config --get user.two >expected.two
  git config --get user.three >expected.three
  git config --get user.four >expected.four
  git config --get user.five >expected.five
)

git init --bare globs
(cd globs
  printf "[user]\ndss = yes\n" >double-star-start
  printf "[user]\ndse = yes\n" >double-star-end
  printf "[user]\ndsm = yes\n" >double-star-middle
  printf "[user]\nssm = yes\n" >single-star-middle
  printf "[user]\nno = no\n" >no

  cat >>config <<-EOF
  [remote "foo"]
    url = https://foo/bar/baz
  [includeIf "hasconfig:remote.*.url:**/baz"]
    path = "double-star-start"
  [includeIf "hasconfig:remote.*.url:**/nomatch"]
    path = "no"
  [includeIf "hasconfig:remote.*.url:https:/**"]
    path = "double-star-end"
  [includeIf "hasconfig:remote.*.url:nomatch:/**"]
    path = "no"
  [includeIf "hasconfig:remote.*.url:https:/**/baz"]
    path = "double-star-middle"
  [includeIf "hasconfig:remote.*.url:https:/**/nomatch"]
    path = "no"
  [includeIf "hasconfig:remote.*.url:https://*/bar/baz"]
    path = "single-star-middle"
  [includeIf "hasconfig:remote.*.url:https://*/baz"]
    path = "no"
EOF

  git config --get user.dss > expected.dss
  git config --get user.dse > expected.dse
  git config --get user.dsm > expected.dsm
  git config --get user.ssm > expected.ssm
)


git init --bare cycle-breaker-direct
(cd cycle-breaker-direct
  cat >include-with-url <<-\EOF
  [remote "bar"]
    url = barurl
EOF
  cat >>config <<-EOF
  [include]
    path = "include-with-url"
  [includeIf "hasconfig:remote.*.url:foourl"]
    path = "include-with-url"
  [include]
    path = "include-with-url"
EOF
)

git init --bare cycle-breaker-indirect
(cd cycle-breaker-indirect
  cat >include-with-url <<-\EOF
  [include]
    path = indirect
EOF
  cat >indirect <<-\EOF
  [remote "bar"]
    url = barurl
EOF
  cat >>config <<-EOF
  [include]
    path = "include-with-url"
  [includeIf "hasconfig:remote.*.url:foourl"]
    path = "include-with-url"
  [include]
    path = "include-with-url"
EOF
)

git init --bare no-cycle
(cd no-cycle
  cat >include-with-url <<-\EOF
  [user]
    name = "works"
EOF
  cat >remote <<-\EOF
  [remote "bar"]
    url = barurl
EOF
  cat >>config <<-EOF
  [include]
    path = "remote"
  [includeIf "hasconfig:remote.*.url:barurl"]
    path = "include-with-url"
  [include]
    path = "remote"
EOF
  git config --get user.name > expected
)
