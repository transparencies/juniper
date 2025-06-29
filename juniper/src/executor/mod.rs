//! Resolve the document to values

use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::HashMap,
    fmt::{Debug, Display},
    sync::{Arc, RwLock},
};

use arcstr::ArcStr;
use fnv::FnvHashMap;
use futures::Stream;

use crate::{
    GraphQLError,
    ast::{
        Definition, Document, Fragment, FromInputValue, InputValue, Operation, OperationType,
        Selection, ToInputValue, Type,
    },
    parser::{SourcePosition, Spanning},
    schema::{
        meta::{
            Argument, DeprecationStatus, EnumMeta, EnumValue, Field, InputObjectMeta,
            InterfaceMeta, ListMeta, MetaType, NullableMeta, ObjectMeta, PlaceholderMeta,
            ScalarMeta, UnionMeta,
        },
        model::{RootNode, SchemaType, TypeType},
    },
    types::{
        async_await::{GraphQLTypeAsync, GraphQLValueAsync},
        base::{GraphQLType, GraphQLValue},
        name::Name,
        subscriptions::{GraphQLSubscriptionType, GraphQLSubscriptionValue},
    },
    value::{DefaultScalarValue, ParseScalarValue, ScalarValue, Value},
};

pub use self::{
    look_ahead::{
        Applies, LookAheadArgument, LookAheadChildren, LookAheadList, LookAheadObject,
        LookAheadSelection, LookAheadValue,
    },
    owned_executor::OwnedExecutor,
};

mod look_ahead;
mod owned_executor;

#[expect(missing_docs, reason = "self-explanatory")]
#[derive(Clone)]
pub enum FieldPath<'a> {
    Root(SourcePosition),
    Field(&'a str, SourcePosition, Arc<FieldPath<'a>>),
}

/// Query execution engine
///
/// The executor helps drive the query execution in a schema. It keeps track
/// of the current field stack, context, variables, and errors.
pub struct Executor<'r, 'a, CtxT, S = DefaultScalarValue>
where
    CtxT: 'a,
    S: 'a,
{
    fragments: &'r HashMap<&'a str, Fragment<'a, S>>,
    variables: &'r Variables<S>,
    current_selection_set: Option<&'r [Selection<'a, S>]>,
    parent_selection_set: Option<&'r [Selection<'a, S>]>,
    current_type: TypeType<'a, S>,
    schema: &'a SchemaType<S>,
    context: &'a CtxT,
    errors: &'r RwLock<Vec<ExecutionError<S>>>,
    field_path: Arc<FieldPath<'a>>,
}

/// Error type for errors that occur during query execution
///
/// All execution errors contain the source position in the query of the field
/// that failed to resolve. It also contains the field stack.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionError<S> {
    location: SourcePosition,
    path: Vec<String>,
    error: FieldError<S>,
}

impl<S> Eq for ExecutionError<S> where Self: PartialEq {}

impl<S> ExecutionError<S> {
    /// Construct a new execution error occuring at the beginning of the query
    pub fn at_origin(error: FieldError<S>) -> ExecutionError<S> {
        ExecutionError {
            location: SourcePosition::new_origin(),
            path: Vec::new(),
            error,
        }
    }
}

impl<S> PartialOrd for ExecutionError<S>
where
    Self: PartialEq,
{
    fn partial_cmp(&self, other: &ExecutionError<S>) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<S> Ord for ExecutionError<S>
where
    Self: Eq,
{
    fn cmp(&self, other: &ExecutionError<S>) -> Ordering {
        (&self.location, &self.path, &self.error.message).cmp(&(
            &other.location,
            &other.path,
            &other.error.message,
        ))
    }
}

/// Error type for errors that occur during field resolution
///
/// Field errors are represented by a human-readable error message and an
/// optional `Value` structure containing additional information.
///
/// They can be converted to from any type that implements `std::fmt::Display`,
/// which makes error chaining with the `?` operator a breeze:
///
/// ```rust
/// # use juniper::{FieldError, ScalarValue};
/// fn get_string(data: Vec<u8>) -> Result<String, FieldError>
/// {
///     let s = String::from_utf8(data)?;
///     Ok(s)
/// }
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct FieldError<S = DefaultScalarValue> {
    message: String,
    extensions: Value<S>,
}

impl<T: Display, S> From<T> for FieldError<S> {
    fn from(e: T) -> Self {
        Self {
            message: e.to_string(),
            extensions: Value::Null,
        }
    }
}

impl<S> FieldError<S> {
    /// Construct a new [`FieldError`] with additional data.
    ///
    /// You can use the [`graphql_value!`] macro for construction:
    /// ```rust
    /// use juniper::{graphql_value, FieldError};
    ///
    /// # let _: FieldError =
    /// FieldError::new(
    ///     "Could not open connection to the database",
    ///     graphql_value!({"internal_error": "Connection refused"}),
    /// );
    /// ```
    ///
    /// The `extensions` parameter will be added to the `"extensions"` field of
    /// the `"errors"` object in response:
    /// ```json
    /// {
    ///   "errors": [
    ///     "message": "Could not open connection to the database",
    ///     "locations": [{"line": 2, "column": 4}],
    ///     "extensions": {
    ///       "internal_error": "Connection refused"
    ///     }
    ///   ]
    /// }
    /// ```
    ///
    /// If the argument is [`Value::Null`], then no extra data will be included.
    ///
    /// [`graphql_value!`]: macro@crate::graphql_value
    #[must_use]
    pub fn new<T: Display>(e: T, extensions: Value<S>) -> Self {
        Self {
            message: e.to_string(),
            extensions,
        }
    }

    /// Returns `"message"` field of this [`FieldError`].
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns `"extensions"` field of this [`FieldError`].
    ///
    /// If there is no `"extensions"`, then [`Value::Null`] will be returned.
    #[must_use]
    pub fn extensions(&self) -> &Value<S> {
        &self.extensions
    }

    /// Maps the [`ScalarValue`] type of this [`FieldError`] into the specified
    /// one.
    #[must_use]
    pub fn map_scalar_value<T>(self) -> FieldError<T>
    where
        S: ScalarValue,
        T: ScalarValue,
    {
        FieldError {
            message: self.message,
            extensions: self.extensions.map_scalar_value(),
        }
    }

    /// Maps the [`FieldError::message`] with the given function.
    #[must_use]
    pub fn map_message(self, f: impl FnOnce(String) -> String) -> Self {
        Self {
            message: f(self.message),
            extensions: self.extensions,
        }
    }
}

/// The result of resolving the value of a field of type `T`
pub type FieldResult<T, S = DefaultScalarValue> = Result<T, FieldError<S>>;

/// The result of resolving an unspecified field
pub type ExecutionResult<S = DefaultScalarValue> = Result<Value<S>, FieldError<S>>;

/// Boxed `Stream` yielding `Result<Value<S>, ExecutionError<S>>`
pub type ValuesStream<'a, S = DefaultScalarValue> =
    std::pin::Pin<Box<dyn Stream<Item = Result<Value<S>, ExecutionError<S>>> + Send + 'a>>;

/// The map of variables used for substitution during query execution
pub type Variables<S = DefaultScalarValue> = HashMap<String, InputValue<S>>;

/// Custom error handling trait to enable error types other than [`FieldError`]
/// to be specified as return value.
///
/// Any custom error type should implement this trait to convert itself into a
/// [`FieldError`].
pub trait IntoFieldError<S = DefaultScalarValue> {
    /// Performs the custom conversion into a [`FieldError`].
    #[must_use]
    fn into_field_error(self) -> FieldError<S>;
}

impl<S1: ScalarValue, S2: ScalarValue> IntoFieldError<S2> for FieldError<S1> {
    fn into_field_error(self) -> FieldError<S2> {
        self.map_scalar_value()
    }
}

impl<S> IntoFieldError<S> for std::convert::Infallible {
    fn into_field_error(self) -> FieldError<S> {
        match self {}
    }
}

impl<S> IntoFieldError<S> for &str {
    fn into_field_error(self) -> FieldError<S> {
        FieldError::<S>::from(self)
    }
}

impl<S> IntoFieldError<S> for String {
    fn into_field_error(self) -> FieldError<S> {
        FieldError::<S>::from(self)
    }
}

impl<S> IntoFieldError<S> for Cow<'_, str> {
    fn into_field_error(self) -> FieldError<S> {
        FieldError::<S>::from(self)
    }
}

impl<S> IntoFieldError<S> for Box<str> {
    fn into_field_error(self) -> FieldError<S> {
        FieldError::<S>::from(self)
    }
}

#[doc(hidden)]
pub trait IntoResolvable<'a, S, T, C>
where
    T: GraphQLValue<S>,
    S: ScalarValue,
{
    type Type;

    #[doc(hidden)]
    fn into_resolvable(self, ctx: &'a C) -> FieldResult<Option<(&'a T::Context, T)>, S>;
}

impl<'a, S, T, C> IntoResolvable<'a, S, T, C> for T
where
    T: GraphQLValue<S>,
    S: ScalarValue,
    T::Context: FromContext<C>,
{
    type Type = T;

    fn into_resolvable(self, ctx: &'a C) -> FieldResult<Option<(&'a T::Context, T)>, S> {
        Ok(Some((FromContext::from(ctx), self)))
    }
}

impl<'a, S, T, C, E: IntoFieldError<S>> IntoResolvable<'a, S, T, C> for Result<T, E>
where
    S: ScalarValue,
    T: GraphQLValue<S>,
    T::Context: FromContext<C>,
{
    type Type = T;

    fn into_resolvable(self, ctx: &'a C) -> FieldResult<Option<(&'a T::Context, T)>, S> {
        self.map(|v: T| Some((<T::Context as FromContext<C>>::from(ctx), v)))
            .map_err(IntoFieldError::into_field_error)
    }
}

impl<'a, S, T, C> IntoResolvable<'a, S, T, C> for (&'a T::Context, T)
where
    S: ScalarValue,
    T: GraphQLValue<S>,
{
    type Type = T;

    fn into_resolvable(self, _: &'a C) -> FieldResult<Option<(&'a T::Context, T)>, S> {
        Ok(Some(self))
    }
}

impl<'a, S, T, C> IntoResolvable<'a, S, Option<T>, C> for Option<(&'a T::Context, T)>
where
    S: ScalarValue,
    T: GraphQLValue<S>,
{
    type Type = T;

    fn into_resolvable(self, _: &'a C) -> FieldResult<Option<(&'a T::Context, Option<T>)>, S> {
        Ok(self.map(|(ctx, v)| (ctx, Some(v))))
    }
}

impl<'a, S1, S2, T, C> IntoResolvable<'a, S2, T, C> for FieldResult<(&'a T::Context, T), S1>
where
    S1: ScalarValue,
    S2: ScalarValue,
    T: GraphQLValue<S2>,
{
    type Type = T;

    fn into_resolvable(self, _: &'a C) -> FieldResult<Option<(&'a T::Context, T)>, S2> {
        self.map(Some).map_err(FieldError::map_scalar_value)
    }
}

impl<'a, S1, S2, T, C> IntoResolvable<'a, S2, Option<T>, C>
    for FieldResult<Option<(&'a T::Context, T)>, S1>
where
    S1: ScalarValue,
    S2: ScalarValue,
    T: GraphQLValue<S2>,
{
    type Type = T;

    fn into_resolvable(self, _: &'a C) -> FieldResult<Option<(&'a T::Context, Option<T>)>, S2> {
        self.map(|o| o.map(|(ctx, v)| (ctx, Some(v))))
            .map_err(FieldError::map_scalar_value)
    }
}

/// Conversion trait for context types
///
/// Used to support different context types for different parts of an
/// application. By making each `GraphQL` type only aware of as much
/// context as it needs to, isolation and robustness can be
/// improved. Implement this trait if you have contexts that can
/// generally be converted between each other.
///
/// The empty tuple `()` can be converted into from any context type,
/// making it suitable for `GraphQL` that don't need _any_ context to
/// work, e.g. scalars or enums.
pub trait FromContext<T> {
    /// Perform the conversion
    fn from(value: &T) -> &Self;
}

/// Marker trait for types that can act as context objects for `GraphQL` types.
pub trait Context {}

impl<C: Context> Context for &C {}

static NULL_CONTEXT: () = ();

impl<T> FromContext<T> for () {
    fn from(_: &T) -> &Self {
        &NULL_CONTEXT
    }
}

impl<T> FromContext<T> for T
where
    T: Context,
{
    fn from(value: &T) -> &Self {
        value
    }
}

impl<'r, 'a, CtxT, S> Executor<'r, 'a, CtxT, S>
where
    S: ScalarValue,
{
    /// Resolve a single arbitrary value into a stream of [`Value`]s.
    /// If a field fails to resolve, pushes error to `Executor`
    /// and returns `Value::Null`.
    pub async fn resolve_into_stream<'i, 'v, 'res, T>(
        &'r self,
        info: &'i T::TypeInfo,
        value: &'v T,
    ) -> Value<ValuesStream<'res, S>>
    where
        'i: 'res,
        'v: 'res,
        'a: 'res,
        T: GraphQLSubscriptionValue<S, Context = CtxT> + ?Sized,
        T::TypeInfo: Sync,
        CtxT: Sync,
        S: Send + Sync,
    {
        self.subscribe(info, value).await.unwrap_or_else(|e| {
            self.push_error(e);
            Value::Null
        })
    }

    /// Resolve a single arbitrary value into a stream of [`Value`]s.
    /// Calls `resolve_into_stream` on `T`.
    pub async fn subscribe<'s, 't, 'res, T>(
        &'r self,
        info: &'t T::TypeInfo,
        value: &'t T,
    ) -> Result<Value<ValuesStream<'res, S>>, FieldError<S>>
    where
        't: 'res,
        'a: 'res,
        T: GraphQLSubscriptionValue<S, Context = CtxT> + ?Sized,
        T::TypeInfo: Sync,
        CtxT: Sync,
        S: Send + Sync,
    {
        value.resolve_into_stream(info, self).await
    }

    /// Resolve a single arbitrary value, mapping the context to a new type
    pub fn resolve_with_ctx<NewCtxT, T>(&self, info: &T::TypeInfo, value: &T) -> ExecutionResult<S>
    where
        NewCtxT: FromContext<CtxT>,
        T: GraphQLValue<S, Context = NewCtxT> + ?Sized,
    {
        self.replaced_context(<NewCtxT as FromContext<CtxT>>::from(self.context))
            .resolve(info, value)
    }

    /// Resolve a single arbitrary value into an `ExecutionResult`
    pub fn resolve<T>(&self, info: &T::TypeInfo, value: &T) -> ExecutionResult<S>
    where
        T: GraphQLValue<S, Context = CtxT> + ?Sized,
    {
        value.resolve(info, self.current_selection_set, self)
    }

    /// Resolve a single arbitrary value into an `ExecutionResult`
    pub async fn resolve_async<T>(&self, info: &T::TypeInfo, value: &T) -> ExecutionResult<S>
    where
        T: GraphQLValueAsync<S, Context = CtxT> + ?Sized,
        T::TypeInfo: Sync,
        CtxT: Sync,
        S: Send + Sync,
    {
        value
            .resolve_async(info, self.current_selection_set, self)
            .await
    }

    /// Resolve a single arbitrary value, mapping the context to a new type
    pub async fn resolve_with_ctx_async<NewCtxT, T>(
        &self,
        info: &T::TypeInfo,
        value: &T,
    ) -> ExecutionResult<S>
    where
        T: GraphQLValueAsync<S, Context = NewCtxT> + ?Sized,
        T::TypeInfo: Sync,
        NewCtxT: FromContext<CtxT> + Sync,
        S: Send + Sync,
    {
        let e = self.replaced_context(<NewCtxT as FromContext<CtxT>>::from(self.context));
        e.resolve_async(info, value).await
    }

    /// Resolve a single arbitrary value into a return value
    ///
    /// If the field fails to resolve, `null` will be returned.
    pub fn resolve_into_value<T>(&self, info: &T::TypeInfo, value: &T) -> Value<S>
    where
        T: GraphQLValue<S, Context = CtxT>,
    {
        self.resolve(info, value).unwrap_or_else(|e| {
            self.push_error(e);
            Value::null()
        })
    }

    /// Resolve a single arbitrary value into a return value
    ///
    /// If the field fails to resolve, `null` will be returned.
    pub async fn resolve_into_value_async<T>(&self, info: &T::TypeInfo, value: &T) -> Value<S>
    where
        T: GraphQLValueAsync<S, Context = CtxT> + ?Sized,
        T::TypeInfo: Sync,
        CtxT: Sync,
        S: Send + Sync,
    {
        self.resolve_async(info, value).await.unwrap_or_else(|e| {
            self.push_error(e);
            Value::null()
        })
    }

    /// Derive a new executor by replacing the context
    ///
    /// This can be used to connect different types, e.g. from different Rust
    /// libraries, that require different context types.
    pub fn replaced_context<'b, NewCtxT>(
        &'b self,
        ctx: &'b NewCtxT,
    ) -> Executor<'b, 'b, NewCtxT, S> {
        Executor {
            fragments: self.fragments,
            variables: self.variables,
            current_selection_set: self.current_selection_set,
            parent_selection_set: self.parent_selection_set,
            current_type: self.current_type.clone(),
            schema: self.schema,
            context: ctx,
            errors: self.errors,
            field_path: self.field_path.clone(),
        }
    }

    #[doc(hidden)]
    pub fn field_sub_executor<'s>(
        &'s self,
        field_alias: &'a str,
        field_name: &'s str,
        location: SourcePosition,
        selection_set: Option<&'s [Selection<'a, S>]>,
    ) -> Executor<'s, 'a, CtxT, S> {
        Executor {
            fragments: self.fragments,
            variables: self.variables,
            current_selection_set: selection_set,
            parent_selection_set: self.current_selection_set,
            current_type: self.schema.make_type(
                &self
                    .current_type
                    .innermost_concrete()
                    .field_by_name(field_name)
                    .expect("Field not found on inner type")
                    .field_type,
            ),
            schema: self.schema,
            context: self.context,
            errors: self.errors,
            field_path: Arc::new(FieldPath::Field(
                field_alias,
                location,
                Arc::clone(&self.field_path),
            )),
        }
    }

    #[doc(hidden)]
    pub fn type_sub_executor<'s>(
        &'s self,
        type_name: Option<&'s str>,
        selection_set: Option<&'s [Selection<'a, S>]>,
    ) -> Executor<'s, 'a, CtxT, S> {
        Executor {
            fragments: self.fragments,
            variables: self.variables,
            current_selection_set: selection_set,
            parent_selection_set: self.current_selection_set,
            current_type: match type_name {
                Some(type_name) => self.schema.type_by_name(type_name).expect("Type not found"),
                None => self.current_type.clone(),
            },
            schema: self.schema,
            context: self.context,
            errors: self.errors,
            field_path: self.field_path.clone(),
        }
    }

    /// `Executor`'s current selection set
    pub(crate) fn current_selection_set(&self) -> Option<&[Selection<'a, S>]> {
        self.current_selection_set
    }

    /// Access the current context
    ///
    /// You usually provide the context when calling the top-level `execute`
    /// function, or using the context factory.
    pub fn context(&self) -> &'r CtxT {
        self.context
    }

    /// The currently executing schema
    pub fn schema(&self) -> &'a SchemaType<S> {
        self.schema
    }

    #[doc(hidden)]
    pub fn current_type(&self) -> &TypeType<'a, S> {
        &self.current_type
    }

    #[doc(hidden)]
    pub fn variables(&self) -> &'r Variables<S> {
        self.variables
    }

    #[doc(hidden)]
    pub fn fragment_by_name<'s>(&'s self, name: &str) -> Option<&'s Fragment<'a, S>> {
        self.fragments.get(name)
    }

    /// The current location of the executor
    pub fn location(&self) -> &SourcePosition {
        self.field_path.location()
    }

    /// Add an error to the execution engine at the current executor location
    pub fn push_error(&self, error: FieldError<S>) {
        self.push_error_at(error, *self.location());
    }

    /// Add an error to the execution engine at a specific location
    pub fn push_error_at(&self, error: FieldError<S>, location: SourcePosition) {
        let mut path = Vec::new();
        self.field_path.construct_path(&mut path);

        let mut errors = self.errors.write().unwrap();

        errors.push(ExecutionError {
            location,
            path,
            error,
        });
    }

    /// Returns new [`ExecutionError`] at current location
    pub fn new_error(&self, error: FieldError<S>) -> ExecutionError<S> {
        let mut path = Vec::new();
        self.field_path.construct_path(&mut path);

        ExecutionError {
            location: *self.location(),
            path,
            error,
        }
    }

    /// Construct a lookahead selection for the current selection.
    ///
    /// This allows seeing the whole selection and perform operations
    /// affecting the children.
    pub fn look_ahead(&'a self) -> LookAheadSelection<'a, S> {
        let field_name = match *self.field_path {
            FieldPath::Field(x, ..) => x,
            FieldPath::Root(_) => unreachable!(),
        };
        self.parent_selection_set
            .and_then(|p| {
                // Search the parent's fields to find this field within the selection set.
                p.iter().find_map(|x| {
                    match x {
                        Selection::Field(field) => {
                            let field = &field.item;
                            // TODO: support excludes.
                            let name = field.name.item;
                            let alias = field.alias.as_ref().map(|a| a.item);

                            (alias.unwrap_or(name) == field_name).then(|| {
                                LookAheadSelection::new(
                                    look_ahead::SelectionSource::Field(field),
                                    self.variables,
                                    self.fragments,
                                )
                            })
                        }
                        Selection::FragmentSpread(_) | Selection::InlineFragment(_) => None,
                    }
                })
            })
            .unwrap_or_else(|| {
                // We didn't find this field in the parent's selection matching it, which means
                // we're inside a `FragmentSpread`.
                LookAheadSelection::new(
                    look_ahead::SelectionSource::Spread {
                        field_name,
                        set: self.current_selection_set,
                    },
                    self.variables,
                    self.fragments,
                )
            })
    }

    /// Create new `OwnedExecutor` and clone all current data
    /// (except for errors) there
    ///
    /// New empty vector is created for `errors` because
    /// existing errors won't be needed to be accessed by user
    /// in OwnedExecutor as existing errors will be returned in
    /// `execute_query`/`execute_mutation`/`resolve_into_stream`/etc.
    pub fn as_owned_executor(&self) -> OwnedExecutor<'a, CtxT, S> {
        OwnedExecutor {
            fragments: self.fragments.clone(),
            variables: self.variables.clone(),
            current_selection_set: self.current_selection_set.map(|x| x.to_vec()),
            parent_selection_set: self.parent_selection_set.map(|x| x.to_vec()),
            current_type: self.current_type.clone(),
            schema: self.schema,
            context: self.context,
            errors: RwLock::new(vec![]),
            field_path: Arc::clone(&self.field_path),
        }
    }
}

impl FieldPath<'_> {
    fn construct_path(&self, acc: &mut Vec<String>) {
        match self {
            FieldPath::Root(_) => (),
            FieldPath::Field(name, _, parent) => {
                parent.construct_path(acc);
                acc.push((*name).into());
            }
        }
    }

    fn location(&self) -> &SourcePosition {
        match *self {
            FieldPath::Root(ref pos) | FieldPath::Field(_, ref pos, _) => pos,
        }
    }
}

impl<S> ExecutionError<S> {
    #[doc(hidden)]
    pub fn new(location: SourcePosition, path: &[&str], error: FieldError<S>) -> ExecutionError<S> {
        ExecutionError {
            location,
            path: path.iter().map(|s| (*s).into()).collect(),
            error,
        }
    }

    /// The error message
    pub fn error(&self) -> &FieldError<S> {
        &self.error
    }

    /// The source location _in the query_ of the field that failed to resolve
    pub fn location(&self) -> &SourcePosition {
        &self.location
    }

    /// The path of fields leading to the field that generated this error
    pub fn path(&self) -> &[String] {
        &self.path
    }
}

/// Create new `Executor` and start query/mutation execution.
/// Returns `IsSubscription` error if subscription is passed.
pub fn execute_validated_query<'b, QueryT, MutationT, SubscriptionT, S>(
    document: &'b Document<S>,
    operation: &'b Spanning<Operation<S>>,
    root_node: &RootNode<QueryT, MutationT, SubscriptionT, S>,
    variables: &Variables<S>,
    context: &QueryT::Context,
) -> Result<(Value<S>, Vec<ExecutionError<S>>), GraphQLError>
where
    S: ScalarValue,
    QueryT: GraphQLType<S>,
    MutationT: GraphQLType<S, Context = QueryT::Context>,
    SubscriptionT: GraphQLType<S, Context = QueryT::Context>,
{
    if operation.item.operation_type == OperationType::Subscription {
        return Err(GraphQLError::IsSubscription);
    }

    let mut fragments = vec![];
    for def in document.iter() {
        if let Definition::Fragment(f) = def {
            fragments.push(f)
        };
    }

    let default_variable_values = operation.item.variable_definitions.as_ref().map(|defs| {
        defs.item
            .items
            .iter()
            .filter_map(|(name, def)| {
                def.default_value
                    .as_ref()
                    .map(|i| (name.item.into(), i.item.clone()))
            })
            .collect::<HashMap<String, InputValue<S>>>()
    });

    let errors = RwLock::new(Vec::new());
    let value;

    {
        let mut all_vars;
        let mut final_vars = variables;

        if let Some(defaults) = default_variable_values {
            all_vars = variables.clone();

            for (name, value) in defaults {
                all_vars.entry(name).or_insert(value);
            }

            final_vars = &all_vars;
        }

        let root_type = match operation.item.operation_type {
            OperationType::Query => root_node.schema.query_type(),
            OperationType::Mutation => root_node
                .schema
                .mutation_type()
                .expect("No mutation type found"),
            OperationType::Subscription => unreachable!(),
        };

        let executor = Executor {
            fragments: &fragments
                .iter()
                .map(|f| (f.item.name.item, f.item.clone()))
                .collect(),
            variables: final_vars,
            current_selection_set: Some(&operation.item.selection_set[..]),
            parent_selection_set: None,
            current_type: root_type,
            schema: &root_node.schema,
            context,
            errors: &errors,
            field_path: Arc::new(FieldPath::Root(operation.span.start)),
        };

        value = match operation.item.operation_type {
            OperationType::Query => executor.resolve_into_value(&root_node.query_info, &root_node),
            OperationType::Mutation => {
                executor.resolve_into_value(&root_node.mutation_info, &root_node.mutation_type)
            }
            OperationType::Subscription => unreachable!(),
        };
    }

    let mut errors = errors.into_inner().unwrap();
    errors.sort();

    Ok((value, errors))
}

/// Create new `Executor` and start asynchronous query execution.
/// Returns `IsSubscription` error if subscription is passed.
pub async fn execute_validated_query_async<'b, QueryT, MutationT, SubscriptionT, S>(
    document: &'b Document<'_, S>,
    operation: &'b Spanning<Operation<'_, S>>,
    root_node: &RootNode<QueryT, MutationT, SubscriptionT, S>,
    variables: &Variables<S>,
    context: &QueryT::Context,
) -> Result<(Value<S>, Vec<ExecutionError<S>>), GraphQLError>
where
    QueryT: GraphQLTypeAsync<S>,
    QueryT::TypeInfo: Sync,
    QueryT::Context: Sync,
    MutationT: GraphQLTypeAsync<S, Context = QueryT::Context>,
    MutationT::TypeInfo: Sync,
    SubscriptionT: GraphQLType<S, Context = QueryT::Context> + Sync,
    SubscriptionT::TypeInfo: Sync,
    S: ScalarValue + Send + Sync,
{
    if operation.item.operation_type == OperationType::Subscription {
        return Err(GraphQLError::IsSubscription);
    }

    let mut fragments = vec![];
    for def in document.iter() {
        if let Definition::Fragment(f) = def {
            fragments.push(f)
        };
    }

    let default_variable_values = operation.item.variable_definitions.as_ref().map(|defs| {
        defs.item
            .items
            .iter()
            .filter_map(|(name, def)| {
                def.default_value
                    .as_ref()
                    .map(|i| (name.item.into(), i.item.clone()))
            })
            .collect::<HashMap<String, InputValue<S>>>()
    });

    let errors = RwLock::new(Vec::new());
    let value;

    {
        let mut all_vars;
        let mut final_vars = variables;

        if let Some(defaults) = default_variable_values {
            all_vars = variables.clone();

            for (name, value) in defaults {
                all_vars.entry(name).or_insert(value);
            }

            final_vars = &all_vars;
        }

        let root_type = match operation.item.operation_type {
            OperationType::Query => root_node.schema.query_type(),
            OperationType::Mutation => root_node
                .schema
                .mutation_type()
                .expect("No mutation type found"),
            OperationType::Subscription => unreachable!(),
        };

        let executor = Executor {
            fragments: &fragments
                .iter()
                .map(|f| (f.item.name.item, f.item.clone()))
                .collect(),
            variables: final_vars,
            current_selection_set: Some(&operation.item.selection_set[..]),
            parent_selection_set: None,
            current_type: root_type,
            schema: &root_node.schema,
            context,
            errors: &errors,
            field_path: Arc::new(FieldPath::Root(operation.span.start)),
        };

        value = match operation.item.operation_type {
            OperationType::Query => {
                executor
                    .resolve_into_value_async(&root_node.query_info, &root_node)
                    .await
            }
            OperationType::Mutation => {
                executor
                    .resolve_into_value_async(&root_node.mutation_info, &root_node.mutation_type)
                    .await
            }
            OperationType::Subscription => unreachable!(),
        };
    }

    let mut errors = errors.into_inner().unwrap();
    errors.sort();

    Ok((value, errors))
}

#[doc(hidden)]
pub fn get_operation<'b, 'd, S>(
    document: &'b Document<'d, S>,
    operation_name: Option<&str>,
) -> Result<&'b Spanning<Operation<'d, S>>, GraphQLError>
where
    S: ScalarValue,
{
    let mut operation = None;
    for def in document {
        if let Definition::Operation(op) = def {
            if operation_name.is_none() && operation.is_some() {
                return Err(GraphQLError::MultipleOperationsProvided);
            }

            let move_op =
                operation_name.is_none() || op.item.name.as_ref().map(|s| s.item) == operation_name;

            if move_op {
                operation = Some(op);
            }
        };
    }
    let op = match operation {
        Some(op) => op,
        None => return Err(GraphQLError::UnknownOperationName),
    };
    Ok(op)
}

/// Initialize new `Executor` and start resolving subscription into stream
/// asynchronously.
/// Returns `NotSubscription` error if query or mutation is passed
pub async fn resolve_validated_subscription<
    'r,
    'exec_ref,
    'd,
    'op,
    QueryT,
    MutationT,
    SubscriptionT,
    S,
>(
    document: &Document<'d, S>,
    operation: &Spanning<Operation<'op, S>>,
    root_node: &'r RootNode<QueryT, MutationT, SubscriptionT, S>,
    variables: &Variables<S>,
    context: &'r QueryT::Context,
) -> Result<(Value<ValuesStream<'r, S>>, Vec<ExecutionError<S>>), GraphQLError>
where
    'r: 'exec_ref,
    'd: 'r,
    'op: 'd,
    QueryT: GraphQLTypeAsync<S>,
    QueryT::TypeInfo: Sync,
    QueryT::Context: Sync + 'r,
    MutationT: GraphQLTypeAsync<S, Context = QueryT::Context>,
    MutationT::TypeInfo: Sync,
    SubscriptionT: GraphQLSubscriptionType<S, Context = QueryT::Context>,
    SubscriptionT::TypeInfo: Sync,
    S: ScalarValue + Send + Sync,
{
    if operation.item.operation_type != OperationType::Subscription {
        return Err(GraphQLError::NotSubscription);
    }

    let mut fragments = vec![];
    for def in document.iter() {
        if let Definition::Fragment(f) = def {
            fragments.push(f)
        }
    }

    let default_variable_values = operation.item.variable_definitions.as_ref().map(|defs| {
        defs.item
            .items
            .iter()
            .filter_map(|(name, def)| {
                def.default_value
                    .as_ref()
                    .map(|i| (name.item.into(), i.item.clone()))
            })
            .collect::<HashMap<String, InputValue<S>>>()
    });

    let errors = RwLock::new(Vec::new());
    let value;

    {
        let mut all_vars;
        let mut final_vars = variables;

        if let Some(defaults) = default_variable_values {
            all_vars = variables.clone();

            for (name, value) in defaults {
                all_vars.entry(name).or_insert(value);
            }

            final_vars = &all_vars;
        }

        let root_type = match operation.item.operation_type {
            OperationType::Subscription => root_node
                .schema
                .subscription_type()
                .expect("No subscription type found"),
            _ => unreachable!(),
        };

        let executor: Executor<'_, 'r, _, _> = Executor {
            fragments: &fragments
                .iter()
                .map(|f| (f.item.name.item, f.item.clone()))
                .collect(),
            variables: final_vars,
            current_selection_set: Some(&operation.item.selection_set[..]),
            parent_selection_set: None,
            current_type: root_type,
            schema: &root_node.schema,
            context,
            errors: &errors,
            field_path: Arc::new(FieldPath::Root(operation.span.start)),
        };

        value = match operation.item.operation_type {
            OperationType::Subscription => {
                executor
                    .resolve_into_stream(&root_node.subscription_info, &root_node.subscription_type)
                    .await
            }
            _ => unreachable!(),
        };
    }

    let mut errors = errors.into_inner().unwrap();
    errors.sort();

    Ok((value, errors))
}

/// A type registry used to build schemas
///
/// The registry gathers metadata for all types in a schema. It provides
/// convenience methods to convert types implementing the `GraphQLType` trait
/// into `Type` instances and automatically registers them.
pub struct Registry<S = DefaultScalarValue> {
    /// Currently registered types
    pub types: FnvHashMap<Name, MetaType<S>>,
}

impl<S> Registry<S> {
    /// Constructs a new [`Registry`] out of the given `types`.
    pub fn new(types: FnvHashMap<Name, MetaType<S>>) -> Self {
        Self { types }
    }

    /// Returns a [`Type`] instance for the given [`GraphQLType`], registered in this [`Registry`].
    ///
    /// If this [`Registry`] hasn't seen a [`Type`] with such [`GraphQLType::name`] before, it will
    /// construct the one and store it.
    pub fn get_type<T>(&mut self, info: &T::TypeInfo) -> Type
    where
        T: GraphQLType<S> + ?Sized,
        S: ScalarValue,
    {
        if let Some(name) = T::name(info) {
            let validated_name = Name::new(name.clone()).unwrap();
            if !self.types.contains_key(&name) {
                self.insert_placeholder(validated_name.clone(), Type::NonNullNamed(name.clone()));
                let meta = T::meta(info, self);
                self.types.insert(validated_name, meta);
            }
            self.types[&name].as_type()
        } else {
            T::meta(info, self).as_type()
        }
    }

    /// Creates a [`Field`] with the provided `name`.
    pub fn field<T>(&mut self, name: impl Into<ArcStr>, info: &T::TypeInfo) -> Field<S>
    where
        T: GraphQLType<S> + ?Sized,
        S: ScalarValue,
    {
        Field {
            name: name.into(),
            description: None,
            arguments: None,
            field_type: self.get_type::<T>(info),
            deprecation_status: DeprecationStatus::Current,
        }
    }

    #[doc(hidden)]
    pub fn field_convert<'a, T: IntoResolvable<'a, S, I, C>, I, C>(
        &mut self,
        name: impl Into<ArcStr>,
        info: &I::TypeInfo,
    ) -> Field<S>
    where
        I: GraphQLType<S>,
        S: ScalarValue,
    {
        Field {
            name: name.into(),
            description: None,
            arguments: None,
            field_type: self.get_type::<I>(info),
            deprecation_status: DeprecationStatus::Current,
        }
    }

    /// Creates an [`Argument`] with the provided `name`.
    pub fn arg<T>(&mut self, name: impl Into<ArcStr>, info: &T::TypeInfo) -> Argument<S>
    where
        T: GraphQLType<S> + FromInputValue<S>,
        S: ScalarValue,
    {
        Argument::new(name, self.get_type::<T>(info))
    }

    /// Creates an [`Argument`] with the provided default `value`.
    pub fn arg_with_default<T>(
        &mut self,
        name: impl Into<ArcStr>,
        value: &T,
        info: &T::TypeInfo,
    ) -> Argument<S>
    where
        T: GraphQLType<S> + ToInputValue<S> + FromInputValue<S>,
        S: ScalarValue,
    {
        Argument::new(name, self.get_type::<T>(info)).default_value(value.to_input_value())
    }

    fn insert_placeholder(&mut self, name: Name, of_type: Type) {
        self.types
            .entry(name)
            .or_insert(MetaType::Placeholder(PlaceholderMeta { of_type }));
    }

    /// Creates a [`ScalarMeta`] type.
    pub fn build_scalar_type<T>(&mut self, info: &T::TypeInfo) -> ScalarMeta<S>
    where
        T: GraphQLType<S> + FromInputValue<S> + ParseScalarValue<S>,
        T::Error: IntoFieldError<S>,
        S: ScalarValue,
    {
        let name = T::name(info).expect("Scalar types must be named. Implement `name()`");

        ScalarMeta::new::<T>(name)
    }

    /// Creates a [`ListMeta`] type.
    ///
    /// Specifying `expected_size` will be used to ensure that values of this
    /// type will always match it.
    pub fn build_list_type<T>(
        &mut self,
        info: &T::TypeInfo,
        expected_size: Option<usize>,
    ) -> ListMeta
    where
        T: GraphQLType<S> + ?Sized,
        S: ScalarValue,
    {
        let of_type = self.get_type::<T>(info);
        ListMeta::new(of_type, expected_size)
    }

    /// Creates a [`NullableMeta`] type.
    pub fn build_nullable_type<T>(&mut self, info: &T::TypeInfo) -> NullableMeta
    where
        T: GraphQLType<S> + ?Sized,
        S: ScalarValue,
    {
        let of_type = self.get_type::<T>(info);
        NullableMeta::new(of_type)
    }

    /// Creates an [`ObjectMeta`] type with the given `fields`.
    pub fn build_object_type<T>(&mut self, info: &T::TypeInfo, fields: &[Field<S>]) -> ObjectMeta<S>
    where
        T: GraphQLType<S> + ?Sized,
        S: ScalarValue,
    {
        let name = T::name(info).expect("Object types must be named. Implement name()");

        let mut v = fields.to_vec();
        v.push(self.field::<String>(arcstr::literal!("__typename"), &()));
        ObjectMeta::new(name, &v)
    }

    /// Creates an [`EnumMeta`] type out of the provided `values`.
    pub fn build_enum_type<T>(&mut self, info: &T::TypeInfo, values: &[EnumValue]) -> EnumMeta<S>
    where
        T: GraphQLType<S> + FromInputValue<S>,
        T::Error: IntoFieldError<S>,
        S: ScalarValue,
    {
        let name = T::name(info).expect("Enum types must be named. Implement `name()`");

        EnumMeta::new::<T>(name, values)
    }

    /// Creates an [`InterfaceMeta`] type with the given `fields`.
    pub fn build_interface_type<T>(
        &mut self,
        info: &T::TypeInfo,
        fields: &[Field<S>],
    ) -> InterfaceMeta<S>
    where
        T: GraphQLType<S> + ?Sized,
        S: ScalarValue,
    {
        let name = T::name(info).expect("Interface types must be named. Implement name()");

        let mut v = fields.to_vec();
        v.push(self.field::<String>(arcstr::literal!("__typename"), &()));
        InterfaceMeta::new(name, &v)
    }

    /// Creates an [`UnionMeta`] type of the given `types`.
    pub fn build_union_type<T>(&mut self, info: &T::TypeInfo, types: &[Type]) -> UnionMeta
    where
        T: GraphQLType<S> + ?Sized,
        S: ScalarValue,
    {
        let name = T::name(info).expect("Union types must be named. Implement name()");

        UnionMeta::new(name, types)
    }

    /// Creates an [`InputObjectMeta`] type with the given `args`.
    pub fn build_input_object_type<T>(
        &mut self,
        info: &T::TypeInfo,
        args: &[Argument<S>],
    ) -> InputObjectMeta<S>
    where
        T: GraphQLType<S> + FromInputValue<S>,
        T::Error: IntoFieldError<S>,
        S: ScalarValue,
    {
        let name = T::name(info).expect("Input object types must be named. Implement name()");

        InputObjectMeta::new::<T>(name, args)
    }
}
