#![cfg(target_os = "windows")]

mod common;
use common::*;

#[test]
fn choco_q() {
    test_dsl! { r##"
        in -Q
        ou Chocolatey
        in -Q choco
        ou Chocolatey
    "## }
}

#[test]
fn choco_qi() {
    test_dsl! { r##"
        in -Qi wget
        ou GNU Wget is a free software package
    "## }
}

#[test]
#[should_panic(expected = "Failed with pattern `GNU Wget is not a free software package`")]
fn choco_fail() {
    test_dsl! { r##"
        in -Si wget
        ou GNU Wget is not a free software package
    "## }
}

#[test]
#[ignore]
fn choco_r_s() {
    test_dsl! { r##"
        in -S wget --yes
        ou The install of wget was successful.
        in -R wget --yes
        ou Wget has been successfully uninstalled.
    "## }
}

#[test]
fn choco_si() {
    test_dsl! { r##"
        in -Si wget
        ou GNU Wget is a free software package
    "## }
}

#[test]
fn choco_ss() {
    test_dsl! { r##"
        in -Ss wget
        ou Wget
    "## }
}
