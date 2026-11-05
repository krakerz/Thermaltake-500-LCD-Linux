//! IPC handlers of the profile requests.

use tt500_lcd_ipc::{
    ContentKind, ErrorCode, IpcError, Outcome, ProfileReason, ProfileSource, ProfileSpec,
    ProfilesReply, Request, Status,
};

use crate::server::{self, ServerContext};

fn error(code: ErrorCode, message: &str) -> IpcError {
    IpcError {
        code,
        message: message.to_owned(),
    }
}

/// Answers the profile requests.
///
/// # Errors
/// `BadRequest` for invalid input, `NotFound` for an unknown profile, and whatever applying
/// the profile fails with.
pub fn handle(ctx: &ServerContext, request: Request) -> Result<Outcome, IpcError> {
    let hub = &ctx.profiles;
    match request {
        Request::ProfileList => {
            return Ok(Outcome::Profiles(ProfilesReply {
                profiles: hub.list(),
                status: ctx.status.get(),
            }));
        }
        Request::ProfileSave(params) => {
            let spec = match params.spec {
                Some(spec) => spec,
                None => snapshot(&ctx.status.get())?,
            };
            hub.save(&params.name, spec)?;
        }
        Request::ProfileRename(params) => hub.rename(&params.from, &params.to)?,
        Request::ProfileDelete(params) => hub.delete(&params.name)?,
        Request::ProfileSetTriggers(params) => hub.set_triggers(&params.name, params.triggers)?,
        Request::ProfileReorder(params) => hub.reorder(&params.names)?,
        Request::ProfileReset(params) => {
            let _guard = hub.lock_activation();
            if let Some(spec) = hub.reset(&params.name)? {
                server::apply_profile(ctx, &spec)?;
                hub.record_applied(&params.name, ProfileReason::Manual, true);
            }
        }
        Request::ProfileActivate(params) => {
            let _guard = hub.lock_activation();
            let profile = hub.get(&params.name).ok_or_else(|| {
                error(
                    ErrorCode::NotFound,
                    &format!("no profile named {:?}", params.name),
                )
            })?;
            server::apply_profile(ctx, &profile.spec)?;
            hub.record_applied(&profile.name, ProfileReason::Manual, true);
        }
        _ => return Err(error(ErrorCode::Unsupported, "unsupported request")),
    }
    Ok(Outcome::Result(ctx.status.get()))
}

/// What the panel shows now, as a profile spec.
fn snapshot(status: &Status) -> Result<ProfileSpec, IpcError> {
    let mut spec = ProfileSpec {
        brightness: Some(status.brightness),
        rotation: status.rotation,
        ..ProfileSpec::default()
    };
    let Some(info) = &status.content else {
        return Ok(spec);
    };
    spec.fit = info.fit;
    spec.crop = info.crop;
    spec.fps = info.max_fps;
    spec.source = Some(if info.kind == ContentKind::Live {
        spec.steady = info.steady;
        ProfileSource::Live {
            source: info
                .live
                .clone()
                .ok_or_else(|| error(ErrorCode::Internal, "the live content has no source"))?,
        }
    } else {
        ProfileSource::Static {
            path: info.path.clone().ok_or_else(|| {
                error(
                    ErrorCode::BadRequest,
                    "nothing to save: the content has no source file",
                )
            })?,
        }
    });
    Ok(spec)
}
