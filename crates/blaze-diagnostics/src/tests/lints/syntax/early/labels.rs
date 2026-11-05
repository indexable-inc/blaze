//! `unused_labels` and the warning of a shadowed label, against rustc's UI tests.

use super::ui_core;
use crate::tests::ui;

#[test]
fn unused_labels_and_a_shadowed_label_are_warned_of() {
    let source = r#"// The output should warn when a loop label is not used. However, it
// should also deal with the edge cases where a label is shadowed,
// within nested loops

//@ check-pass

#![warn(unused_labels)]

fn main() {
    'unused_while_label: while 0 == 0 {
        //~^ WARN unused label
    }

    let opt = Some(0);
    'unused_while_let_label: while let Some(_) = opt {
        //~^ WARN unused label
    }

    'unused_for_label: for _ in 0..10 {
        //~^ WARN unused label
    }

    'used_loop_label: loop {
        break 'used_loop_label;
    }

    'used_loop_label_outer_1: for _ in 0..10 {
        'used_loop_label_inner_1: for _ in 0..10 {
            break 'used_loop_label_inner_1;
        }
        break 'used_loop_label_outer_1;
    }

    'used_loop_label_outer_2: for _ in 0..10 {
        'unused_loop_label_inner_2: for _ in 0..10 {
            //~^ WARN unused label
            break 'used_loop_label_outer_2;
        }
    }

    'unused_loop_label_outer_3: for _ in 0..10 {
        //~^ WARN unused label
        'used_loop_label_inner_3: for _ in 0..10 {
            break 'used_loop_label_inner_3;
        }
    }

    // You should be able to break the same label many times
    'many_used: loop {
        if true {
            break 'many_used;
        } else {
            break 'many_used;
        }
    }

    // Test breaking many times with the same inner label doesn't break the
    // warning on the outer label
    'many_used_shadowed: for _ in 0..10 {
        //~^ WARN unused label
        'many_used_shadowed: for _ in 0..10 {
            //~^ WARN label name `'many_used_shadowed` shadows a label name that is already in scope
            if 1 % 2 == 0 {
                break 'many_used_shadowed;
            } else {
                break 'many_used_shadowed;
            }
        }
    }

    'unused_loop_label: loop {
        //~^ WARN unused label
        break;
    }

    // Make sure unused block labels give warnings...
    'unused_block_label: {
        //~^ WARN unused label
    }

    // ...and that used ones don't:
    'used_block_label: {
        break 'used_block_label;
    }
}
"#;
    let stderr = r#"warning: label name `'many_used_shadowed` shadows a label name that is already in scope
  --> $DIR/unused_labels.rs:61:9
   |
LL |     'many_used_shadowed: for _ in 0..10 {
   |     ------------------- first declared here
LL |
LL |         'many_used_shadowed: for _ in 0..10 {
   |         ^^^^^^^^^^^^^^^^^^^ label `'many_used_shadowed` already in scope

warning: unused label
  --> $DIR/unused_labels.rs:10:5
   |
LL |     'unused_while_label: while 0 == 0 {
   |     ^^^^^^^^^^^^^^^^^^^
   |
note: the lint level is defined here
  --> $DIR/unused_labels.rs:7:9
   |
LL | #![warn(unused_labels)]
   |         ^^^^^^^^^^^^^

warning: unused label
  --> $DIR/unused_labels.rs:15:5
   |
LL |     'unused_while_let_label: while let Some(_) = opt {
   |     ^^^^^^^^^^^^^^^^^^^^^^^

warning: unused label
  --> $DIR/unused_labels.rs:19:5
   |
LL |     'unused_for_label: for _ in 0..10 {
   |     ^^^^^^^^^^^^^^^^^

warning: unused label
  --> $DIR/unused_labels.rs:35:9
   |
LL |         'unused_loop_label_inner_2: for _ in 0..10 {
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^

warning: unused label
  --> $DIR/unused_labels.rs:41:5
   |
LL |     'unused_loop_label_outer_3: for _ in 0..10 {
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^

warning: unused label
  --> $DIR/unused_labels.rs:59:5
   |
LL |     'many_used_shadowed: for _ in 0..10 {
   |     ^^^^^^^^^^^^^^^^^^^

warning: unused label
  --> $DIR/unused_labels.rs:71:5
   |
LL |     'unused_loop_label: loop {
   |     ^^^^^^^^^^^^^^^^^^

warning: unused label
  --> $DIR/unused_labels.rs:77:5
   |
LL |     'unused_block_label: {
   |     ^^^^^^^^^^^^^^^^^^^

warning: 9 warnings emitted

"#;
    assert_eq!(ui_core("unused_labels.rs", "2015", "range, iterator, option, eq", source), stderr);
}

#[test]
fn a_label_beginning_with_an_underscore_is_not_unused() {
    let source = r#"//@ check-pass

#![deny(unused_labels)]

fn main() {
    // `unused_label` shouldn't warn labels beginning with `_`
    '_unused: loop {
        break;
    }
}
"#;
    assert_eq!(ui("label-beginning-with-underscore.rs", "2015", source), "");
}
