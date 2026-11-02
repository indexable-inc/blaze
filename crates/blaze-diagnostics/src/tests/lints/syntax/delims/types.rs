//! Parentheses around a type.

use crate::tests::ui;

#[test]
fn a_cast_type_before_a_less_than_or_a_shift_keeps_its_parentheses() {
    let source = r#"//@ check-pass
#![warn(unused_parens)]

fn id<T>(t: T) -> T { t }

fn main() {
    // This should not warn
    let _ = 1 as (i32) < 2;
    let _ = id(1 as (i32) < 2);
    let _ = id(1 as i32)
        as (i32) < 2;
    // These should warn
    let _ = 1 as ((i32)) < 2; //~WARN unnecessary parentheses
    let _ = 1 as (i32); //~WARN unnecessary parentheses
    let _ = 1 as (i32) > 2; //~WARN unnecessary parentheses
    let _ = id(1 as (i32)) //~WARN unnecessary parentheses
        as (i32) < 2;
    let _ = id(1 as (i32)) < 2; //~WARN unnecessary parentheses

    // This should not warn
    let _ = 1 as (i32) << 2;
    let _ = id(1 as (i32) << 2);
    let _ = id(1 as i32)
        as (i32) << 2;
    // These should warn
    let _ = 1 as ((i32)) << 2; //~WARN unnecessary parentheses
    let _ = 1 as (i32); //~WARN unnecessary parentheses
    let _ = 1 as (i32) >> 2; //~WARN unnecessary parentheses
    let _ = id(1 as (i32)) //~WARN unnecessary parentheses
        as (i32) << 2;
    let _ = id(1 as (i32)) << 2; //~WARN unnecessary parentheses
}
"#;
    let expected = r#"warning: unnecessary parentheses around type
  --> $DIR/unused-parens-issue-106413.rs:13:19
   |
LL |     let _ = 1 as ((i32)) < 2;
   |                   ^   ^
   |
note: the lint level is defined here
  --> $DIR/unused-parens-issue-106413.rs:2:9
   |
LL | #![warn(unused_parens)]
   |         ^^^^^^^^^^^^^
help: remove these parentheses
   |
LL -     let _ = 1 as ((i32)) < 2;
LL +     let _ = 1 as (i32) < 2;
   |

warning: unnecessary parentheses around type
  --> $DIR/unused-parens-issue-106413.rs:14:18
   |
LL |     let _ = 1 as (i32);
   |                  ^   ^
   |
help: remove these parentheses
   |
LL -     let _ = 1 as (i32);
LL +     let _ = 1 as i32;
   |

warning: unnecessary parentheses around type
  --> $DIR/unused-parens-issue-106413.rs:15:18
   |
LL |     let _ = 1 as (i32) > 2;
   |                  ^   ^
   |
help: remove these parentheses
   |
LL -     let _ = 1 as (i32) > 2;
LL +     let _ = 1 as i32 > 2;
   |

warning: unnecessary parentheses around type
  --> $DIR/unused-parens-issue-106413.rs:16:21
   |
LL |     let _ = id(1 as (i32))
   |                     ^   ^
   |
help: remove these parentheses
   |
LL -     let _ = id(1 as (i32))
LL +     let _ = id(1 as i32)
   |

warning: unnecessary parentheses around type
  --> $DIR/unused-parens-issue-106413.rs:18:21
   |
LL |     let _ = id(1 as (i32)) < 2;
   |                     ^   ^
   |
help: remove these parentheses
   |
LL -     let _ = id(1 as (i32)) < 2;
LL +     let _ = id(1 as i32) < 2;
   |

warning: unnecessary parentheses around type
  --> $DIR/unused-parens-issue-106413.rs:26:19
   |
LL |     let _ = 1 as ((i32)) << 2;
   |                   ^   ^
   |
help: remove these parentheses
   |
LL -     let _ = 1 as ((i32)) << 2;
LL +     let _ = 1 as (i32) << 2;
   |

warning: unnecessary parentheses around type
  --> $DIR/unused-parens-issue-106413.rs:27:18
   |
LL |     let _ = 1 as (i32);
   |                  ^   ^
   |
help: remove these parentheses
   |
LL -     let _ = 1 as (i32);
LL +     let _ = 1 as i32;
   |

warning: unnecessary parentheses around type
  --> $DIR/unused-parens-issue-106413.rs:28:18
   |
LL |     let _ = 1 as (i32) >> 2;
   |                  ^   ^
   |
help: remove these parentheses
   |
LL -     let _ = 1 as (i32) >> 2;
LL +     let _ = 1 as i32 >> 2;
   |

warning: unnecessary parentheses around type
  --> $DIR/unused-parens-issue-106413.rs:29:21
   |
LL |     let _ = id(1 as (i32))
   |                     ^   ^
   |
help: remove these parentheses
   |
LL -     let _ = id(1 as (i32))
LL +     let _ = id(1 as i32)
   |

warning: unnecessary parentheses around type
  --> $DIR/unused-parens-issue-106413.rs:31:21
   |
LL |     let _ = id(1 as (i32)) << 2;
   |                     ^   ^
   |
help: remove these parentheses
   |
LL -     let _ = id(1 as (i32)) << 2;
LL +     let _ = id(1 as i32) << 2;
   |

warning: 10 warnings emitted

"#;
    assert_eq!(ui("unused-parens-issue-106413.rs", "2015", source), expected);
}
