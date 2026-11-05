# Reintroduce a FCW system to borrowck

| Metadata            |                                    |
| :------------------ | :--------------------------------- |
| Contact    | @oli-obk                           |
| Status              | Accepted                           |
| Funding contact     | [RustNL](https://rustnl.org)       |
| Tracking issue      | [rust-lang/goals#713] |
| Zulip channel       | N/A                                |
| [compiler] champion | @oli-obk                           |
| [types] champion    | @lcnr                              |

## Summary

When borrowck was transitioned to NLL, we had a system for turning some errors into warnings, if they only happened
with NLL but not old borrowck and generally compare two impls of borrowck to make sure the errors match up.

Due to recent changes to borrowck (opaque type handling and borrowcking typeck children individually),
the remains of the system for comparing two runs of borrowck can't easily be used anymore for those purposes.

## Motivation

### The status quo

Fixing soundness bugs sometimes requires breaking changes. We want to carefully roll them out, so we would like to be able
to run borrowck with the new system and if there were any errors, run it *also* with the old system and compare errors.

### What we propose to do about it

I'm going to refactor the diagnostic reporting infra in borrowck and introduce a diagnostic comparison system.

### Work items over the next year

| Task                                                                                          | Owner(s) | Notes                                                                                     |
| --------------------------------------------------------------------------------------------- | -------- | ----------------------------------------------------------------------------------------- |
| link all `Diag<'infcx>` to the borrowck root instead                                          | @oli-obk |                                                                                           |
| delay all diag reporting to the end of borrowck                                               | @oli-obk | may change diagnostic order                                                               |
| add an enum for borrowck modes and a list of borrowck modes to try and compare errors between | @oli-obk | we don't have anything to test this well, so I may need to test it with some dummy change |

## Team asks

| Team       | Support level | Notes |
| ---------- | ------------- | ----- |
| [compiler] | medium        |       |
| [types]    | medium        |       |

## Funding

| Purpose | Cost | Funded  | Sponsor(s) |
| ------- | ---- | ------- | ---------- |
| Reviews | Ask  | Partial |            |

## Frequently asked questions
