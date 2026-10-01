//! .NET-only implementation. `evaluator` and `reload` adapt existing ShojiWM
//! types; hosting, assembly staging and source/build operations are independent.
mod assembly;
mod evaluator;
mod protocol;
pub(crate) mod reload;
mod source;
mod host;
mod process;

pub use evaluator::DotNetDecorationEvaluator;
