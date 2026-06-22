VERSION 0.8

# Per-image Earthly targets that wrap each crate's Dockerfile.
#
# Used by local-dev: `earthly +images` builds every botwork image, or
# `earthly +<svc>-image` builds one. CI does not invoke earthly; the
# per-crate matrix in .github/workflows/_crate.yml calls `docker buildx`
# against the same Dockerfile, so the bytes are identical either way.
#
# `GIT_SHA` is baked into the runtime image's
# `org.opencontainers.image.revision` label. Local-dev passes an empty
# string and gets a label-less image; release.yml + _crate.yml pass
# GITHUB_SHA.

admin-api-image:
    ARG GIT_SHA=""
    FROM DOCKERFILE --platform=linux/amd64 \
        -f admin-api/Dockerfile \
        --build-arg GIT_SHA=${GIT_SHA} \
        .
    SAVE IMAGE botwork/admin-api:local

admin-ui-image:
    ARG GIT_SHA=""
    FROM DOCKERFILE --platform=linux/amd64 \
        -f admin-ui/Dockerfile \
        --build-arg GIT_SHA=${GIT_SHA} \
        .
    SAVE IMAGE botwork/admin-ui:local

session-broker-image:
    ARG GIT_SHA=""
    FROM DOCKERFILE --platform=linux/amd64 \
        -f session-broker/Dockerfile \
        --build-arg GIT_SHA=${GIT_SHA} \
        .
    SAVE IMAGE botwork/session-broker:local

config-broker-image:
    ARG GIT_SHA=""
    FROM DOCKERFILE --platform=linux/amd64 \
        -f config-broker/Dockerfile \
        --build-arg GIT_SHA=${GIT_SHA} \
        .
    SAVE IMAGE botwork/config-broker:local

control-plane-image:
    ARG GIT_SHA=""
    FROM DOCKERFILE --platform=linux/amd64 \
        -f control-plane/Dockerfile \
        --build-arg GIT_SHA=${GIT_SHA} \
        .
    SAVE IMAGE botwork/control-plane:local

db-migrate-image:
    ARG GIT_SHA=""
    FROM DOCKERFILE --platform=linux/amd64 \
        -f db/migration/Dockerfile \
        --build-arg GIT_SHA=${GIT_SHA} \
        .
    SAVE IMAGE botwork/db-migrate:local

bootstrap-image:
    ARG GIT_SHA=""
    FROM DOCKERFILE --platform=linux/amd64 \
        -f bootstrap/Dockerfile \
        --build-arg GIT_SHA=${GIT_SHA} \
        .
    SAVE IMAGE botwork/bootstrap:local

images:
    BUILD +admin-api-image
    BUILD +admin-ui-image
    BUILD +session-broker-image
    BUILD +config-broker-image
    BUILD +control-plane-image
    BUILD +db-migrate-image
    BUILD +bootstrap-image
