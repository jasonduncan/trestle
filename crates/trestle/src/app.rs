//! Apps without a type parameter.

use trestle_core::registry::App as CoreApp;

use crate::State;

/// The registration builder for apps on the default [`State`].
pub type AppBuilder<S = State> = trestle_core::registry::AppBuilder<S>;

/// An app on a project-specific state type (the advanced form).
pub use trestle_core::registry::App as AppOf;

/// A cohesive feature (lessons, billing, …) on the default [`State`].
///
/// ```ignore
/// struct Notes;
/// impl trestle::App for Notes {
///     fn id(&self) -> &'static str { "notes" }
///     fn register(&self, app: &mut trestle::AppBuilder) { /* models, operations */ }
/// }
/// ```
pub trait App: Send + Sync + 'static {
    /// Stable lowercase identifier; also the namespace of what it registers.
    fn id(&self) -> &'static str;

    fn register(&self, app: &mut AppBuilder);
}

/// Adapts an [`App`] to the registry's [`AppOf<State>`].
pub struct Erased<A>(pub A);

impl<A: App> CoreApp<State> for Erased<A> {
    fn id(&self) -> &'static str {
        self.0.id()
    }

    fn register(&self, app: &mut AppBuilder) {
        self.0.register(app);
    }
}
