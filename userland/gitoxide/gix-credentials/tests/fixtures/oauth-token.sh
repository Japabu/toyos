#!/bin/sh
set -eu

test "$1" = get && \
echo oauth_refresh_token=oauth-token
