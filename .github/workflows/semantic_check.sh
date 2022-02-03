#!/usr/bin/env bash

semantic_pattern='^(feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert)(\(|:)'

if [[ -z $BASE_SHA || -z $HEAD_SHA || -z $PR_TITLE ]]; then
  echo ::error::required env vars: BASE_SHA, HEAD_SHA, PR_TITLE
  exit 1
fi

exit_code=0

if [[ ! $PR_TITLE =~ $semantic_pattern ]]; then
  echo ::error::PR title not semantic: "$PR_TITLE"
  exit_code=1
fi

while true; do
  HEAD_SHA=$(git log --format=%H -n 1 HEAD)
  if [[ $BASE_SHA == "$HEAD_SHA" ]]; then break; fi

  commit_title="$(git log --format=%s -n 1 HEAD)"
  if [[ ! $commit_title =~ $semantic_pattern ]]; then
    echo ::error::Commit title not semantic: "$commit_title"
    exit_code=1
  fi

  git checkout HEAD^ --quiet
done

exit $exit_code
