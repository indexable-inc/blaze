// A stand-in for anim's `nuna` member with its manifest: each dependency named.
use base64 as _;
use cosmic_text as _;
use crossterm as _;
use flate2 as _;
use flecs_ecs as _;
use libc as _;
use nu_cli as _;
use nu_cmd_lang as _;
use nu_command as _;
use nu_engine as _;
use nu_parser as _;
use nu_protocol as _;
use nu_utils as _;
use taffy as _;
use tiny_skia as _;

fn main() {
    println!("{}", std::any::type_name::<nu_protocol::Value>());
}
