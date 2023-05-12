# Contributing Guideline

Thank you for your interest in contributing to LNDK! This project is still experimental, but all contributions are welcome and wanted. 

Please note the project's [code of conduct](https://github.com/carlaKC/lndk/blob/master/code_of_conduct.md) if you're interested in helping out.

## Types of Contribution 
Contributions of various kinds are welcome here: 
1. **Review of open pull requests**: [considerate review](https://jonatack.github.io/articles/how-to-review-pull-requests-in-bitcoin-core) of open PRs is a great way to start contributing to the project. PRs tagged with the [review wanted](https://github.com/carlaKC/lndk/labels/review%20wanted) label, and those that have no reviewers assigned are a good starting point.
2. **Feature requests**: Creation of issues for [new features](https://github.com/carlaKC/lndk/issues/new?assignees=&labels=&template=feature_request.md&title=Feature%3A+) or [small tasks](https://github.com/carlaKC/lndk/issues/new?assignees=&labels=&template=task-description.md&title=Task%3A) are a useful way to help guide the project's priorities. 
3. **Code contributions**: Anyone is welcome to "scratch their own itch" and open up a PR implementing a feature they're interested in, or pick up a task labeled with [help wanted](https://github.com/carlaKC/lndk/issues?q=is%3Aissue+is%3Aopen+label%3A%22help+wanted%22) or [good first issue](https://github.com/carlaKC/lndk/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22).
4. **Documentation improvements**: Pull requests or issues aimed at improving documentation are a great way to "pay it forward" to the next confused user that comes along.
5. **Testing and bug reports**: Local testing and [bug report](https://github.com/carlaKC/lndk/issues/new?assignees=&labels=bug&template=bug_report.md&title=Bug%3A) filing is one of the most helpful ways to improve the quality of the project.
6. **Support**: Supporting other users by answering questions posed in issues or bug reports helps cut down maintenance time and share knowledge. 

### Code Contribution 
General guidelines for code contribution:
1. Commits should be logically structured, and pass all tests so that `git bisect` can be used.
2. Refactor commits should be done in their own commits, separately from logical changes.
3. Wherever possible, new code should be covered by tests.
4. Bug fixes should start with a test demonstrating the bug, then add the fix and update the test to illustrate that the bug has been addressed.
5. It is recommended to always use [cargo-crev](https://github.com/crev-dev/cargo-crev) to verify the trustworthiness of any added dependencies, which should be kept to a minimum where possible.
6. Test changes on regtest where possible. See [Github Discussions](https://github.com/carlaKC/lndk/discussions) for guides on setting up local development environments (and other meta topics).

#### Process
0. **Issue creation**: If no issue is open for the work you'd like to implement, please open one as a preliminary step so that other contributors can comment on the proposed approach and suitableness of the feature for the repo.
1. **Issue assignee**: Github's "assignee" field is used to track who's working on a task, to ensure that work is not duplicated. When you want to pick up an issue, comment on it to check whether anybody is working on it and it will be assigned to you.
2. **Pull request**: Open a pull request implementing the feature. Feel free to use github's draft feature to open early to get early, high level feedback on the change.
3. **Review cycle**: 
    1. When your PR is ready for review and passing all CI checks, move it out of draft and add a comment indicating that it is ready for a look.
    2. If review capacity is available, reviewers will be assigned. If nobody is available, it will be tagged with [review wanted](https://github.com/carlaKC/lndk/labels/review%20wanted) and added to the queue.
    3. Reviewers will post comments and suggestions on the PR, be sure to address all comments (either with a code change, or an explanation why you're not changing it) before requesting review again.
    4. When all outstanding comments are addressed, use Github's "request review" feature to restart the review process. Pushes to a PR *will not* be interpreted as a request for review. 

#### Conventions
The following conventions are use for code style (and enforced by the CI): 
1. Rust format: `cargo fmt`
2. Clippy: 
```
rustup component add clippy
cargo clippy
``` 
3. String formatting
    1. Logs: Capitalize and include a `.` terminating.
    2. Error String: Lower case, no `.` terminating.
    3. Display imps: Lower case, no `.` terminating.

