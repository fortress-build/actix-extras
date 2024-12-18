use actix_session::Session;
use actix_utils::future::{ready, Ready};
use actix_web::{
    cookie::time::OffsetDateTime,
    dev::{Extensions, Payload},
    http::StatusCode,
    Error, FromRequest, HttpMessage, HttpRequest, HttpResponse,
};
use serde_json::Value;

use crate::{
    config::LogoutBehaviour,
    error::{
        GetIdentityError, InvalidIdTypeError, LoginError, LostIdentityError, MissingIdentityError,
        SessionExpiryError,
    },
};

/// A verified user identity. It can be used as a request extractor.
///
/// The lifecycle of a user identity is tied to the lifecycle of the underlying session. If the
/// session is destroyed (e.g. the session expired), the user identity will be forgotten, de-facto
/// forcing a user log out.
///
/// # Examples
/// ```
/// use actix_web::{
///     get, post, Responder, HttpRequest, HttpMessage, HttpResponse
/// };
/// use actix_identity::Identity;
///
/// #[get("/")]
/// async fn index(user: Option<Identity>) -> impl Responder {
///     if let Some(user) = user {
///         format!("Welcome! {}", user.id().unwrap())
///     } else {
///         "Welcome Anonymous!".to_owned()
///     }
/// }
///
/// #[post("/login")]
/// async fn login(request: HttpRequest) -> impl Responder {
///     Identity::login(&request.extensions(), "User1".into());
///     HttpResponse::Ok()
/// }
///
/// #[post("/logout")]
/// async fn logout(user: Identity) -> impl Responder {
///     user.logout();
///     HttpResponse::Ok()
/// }
/// ```
///
/// # Extractor Behaviour
/// What happens if you try to extract an `Identity` out of a request that does not have a valid
/// identity attached? The API will return a `401 UNAUTHORIZED` to the caller.
///
/// If you want to customise this behaviour, consider extracting `Option<Identity>` or
/// `Result<Identity, actix_web::Error>` instead of a bare `Identity`: you will then be fully in
/// control of the error path.
///
/// ## Examples
/// ```
/// use actix_web::{http::header::LOCATION, get, HttpResponse, Responder};
/// use actix_identity::Identity;
///
/// #[get("/")]
/// async fn index(user: Option<Identity>) -> impl Responder {
///     if let Some(user) = user {
///         HttpResponse::Ok().finish()
///     } else {
///         // Redirect to login page if unauthenticated
///         HttpResponse::TemporaryRedirect()
///             .insert_header((LOCATION, "/login"))
///             .finish()
///     }
/// }
/// ```
pub struct Identity(IdentityInner);

#[derive(Clone)]
pub(crate) struct IdentityInner {
    pub(crate) session: Session,
    pub(crate) logout_behaviour: LogoutBehaviour,
    pub(crate) is_login_deadline_enabled: bool,
    pub(crate) is_visit_deadline_enabled: bool,
    pub(crate) id_key: &'static str,
    pub(crate) last_visit_unix_timestamp_key: &'static str,
    pub(crate) login_unix_timestamp_key: &'static str,
}

impl IdentityInner {
    fn extract(ext: &Extensions) -> Self {
        ext.get::<Self>()
            .expect(
                "No `IdentityInner` instance was found in the extensions attached to the \
                incoming request. This usually means that `IdentityMiddleware` has not been \
                registered as an application middleware via `App::wrap`. `Identity` cannot be used \
                unless the identity machine is properly mounted: register `IdentityMiddleware` as \
                a middleware for your application to fix this panic. If the problem persists, \
                please file an issue on GitHub.",
            )
            .to_owned()
    }

    /// Retrieve the user id attached to the current session.
    fn get_identity(&self) -> Result<String, GetIdentityError> {
        self.session
            .get_value(self.id_key)
            .ok_or_else(|| MissingIdentityError.into())
            .and_then(|value| match value {
                Value::String(s) => Ok(s),
                Value::Null => Err(InvalidIdTypeError("null").into()),
                Value::Bool(_) => Err(InvalidIdTypeError("bool").into()),
                Value::Number(_) => Err(InvalidIdTypeError("number").into()),
                Value::Array(_) => Err(InvalidIdTypeError("array").into()),
                Value::Object(_) => Err(InvalidIdTypeError("object").into()),
            })
    }
}

impl Identity {
    /// Useful for testing
    pub fn mock(id: String) -> Self {
        let session = Session::mock(Default::default(), actix_session::SessionStatus::Unchanged);

        session.insert("nervemq-id", id).unwrap();

        Self(IdentityInner {
            session,
            logout_behaviour: LogoutBehaviour::PurgeSession,
            is_login_deadline_enabled: false,
            is_visit_deadline_enabled: false,
            id_key: "nervemq-id",
            last_visit_unix_timestamp_key: "last-visit-timestamp",
            login_unix_timestamp_key: "login-timestamp",
        })
    }

    /// Return the user id associated to the current session.
    ///
    /// # Examples
    /// ```
    /// use actix_web::{get, Responder};
    /// use actix_identity::Identity;
    ///
    /// #[get("/")]
    /// async fn index(user: Option<Identity>) -> impl Responder {
    ///     if let Some(user) = user {
    ///         format!("Welcome! {}", user.id().unwrap())
    ///     } else {
    ///         "Welcome Anonymous!".to_owned()
    ///     }
    /// }
    /// ```
    pub fn id(&self) -> Result<String, GetIdentityError> {
        self.0
            .session
            .get(self.0.id_key)?
            .ok_or_else(|| LostIdentityError.into())
    }

    /// Attach a valid user identity to the current session.
    ///
    /// This method should be called after you have successfully authenticated the user. After
    /// `login` has been called, the user will be able to access all routes that require a valid
    /// [`Identity`].
    ///
    /// # Examples
    /// ```
    /// use actix_web::{post, Responder, HttpRequest, HttpMessage, HttpResponse};
    /// use actix_identity::Identity;
    ///
    /// #[post("/login")]
    /// async fn login(request: HttpRequest) -> impl Responder {
    ///     Identity::login(&request.extensions(), "User1".into());
    ///     HttpResponse::Ok()
    /// }
    /// ```
    pub fn login(ext: &Extensions, id: String) -> Result<Self, LoginError> {
        let inner = IdentityInner::extract(ext);
        inner.session.insert(inner.id_key, id)?;
        let now = OffsetDateTime::now_utc().unix_timestamp();
        if inner.is_login_deadline_enabled {
            inner.session.insert(inner.login_unix_timestamp_key, now)?;
        }
        if inner.is_visit_deadline_enabled {
            inner
                .session
                .insert(inner.last_visit_unix_timestamp_key, now)?;
        }
        inner.session.renew();
        Ok(Self(inner))
    }

    /// Remove the user identity from the current session.
    ///
    /// After `logout` has been called, the user will no longer be able to access routes that
    /// require a valid [`Identity`].
    ///
    /// The behaviour on logout is determined by [`IdentityMiddlewareBuilder::logout_behaviour`].
    ///
    /// # Examples
    /// ```
    /// use actix_web::{post, Responder, HttpResponse};
    /// use actix_identity::Identity;
    ///
    /// #[post("/logout")]
    /// async fn logout(user: Identity) -> impl Responder {
    ///     user.logout();
    ///     HttpResponse::Ok()
    /// }
    /// ```
    ///
    /// [`IdentityMiddlewareBuilder::logout_behaviour`]: crate::config::IdentityMiddlewareBuilder::logout_behaviour
    pub fn logout(self) {
        match self.0.logout_behaviour {
            LogoutBehaviour::PurgeSession => {
                self.0.session.purge();
            }
            LogoutBehaviour::DeleteIdentityKeys => {
                self.0.session.remove(self.0.id_key);
                if self.0.is_login_deadline_enabled {
                    self.0.session.remove(self.0.login_unix_timestamp_key);
                }
                if self.0.is_visit_deadline_enabled {
                    self.0.session.remove(self.0.last_visit_unix_timestamp_key);
                }
            }
        }
    }

    pub(crate) fn extract(ext: &Extensions) -> Result<Self, GetIdentityError> {
        let inner = IdentityInner::extract(ext);
        inner.get_identity()?;
        Ok(Self(inner))
    }

    pub(crate) fn logged_at(&self) -> Result<Option<OffsetDateTime>, GetIdentityError> {
        Ok(self
            .0
            .session
            .get(self.0.login_unix_timestamp_key)?
            .map(OffsetDateTime::from_unix_timestamp)
            .transpose()
            .map_err(SessionExpiryError)?)
    }

    pub(crate) fn last_visited_at(&self) -> Result<Option<OffsetDateTime>, GetIdentityError> {
        Ok(self
            .0
            .session
            .get(self.0.last_visit_unix_timestamp_key)?
            .map(OffsetDateTime::from_unix_timestamp)
            .transpose()
            .map_err(SessionExpiryError)?)
    }

    pub(crate) fn set_last_visited_at(&self) -> Result<(), LoginError> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        self.0
            .session
            .insert(self.0.last_visit_unix_timestamp_key, now)?;
        Ok(())
    }
}

/// Extractor implementation for [`Identity`].
///
/// # Examples
/// ```
/// use actix_web::{get, Responder};
/// use actix_identity::Identity;
///
/// #[get("/")]
/// async fn index(user: Option<Identity>) -> impl Responder {
///     if let Some(user) = user {
///         format!("Welcome! {}", user.id().unwrap())
///     } else {
///         "Welcome Anonymous!".to_owned()
///     }
/// }
/// ```
impl FromRequest for Identity {
    type Error = Error;
    type Future = Ready<Result<Self, Self::Error>>;

    #[inline]
    fn from_request(req: &HttpRequest, _: &mut Payload) -> Self::Future {
        ready(Identity::extract(&req.extensions()).map_err(|err| {
            let res = actix_web::error::InternalError::from_response(
                err,
                HttpResponse::new(StatusCode::UNAUTHORIZED),
            );

            actix_web::Error::from(res)
        }))
    }
}
