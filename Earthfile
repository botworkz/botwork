VERSION 0.8

admin-api-image:
    ARG GIT_SHA=""
    FROM DOCKERFILE --platform=linux/amd64 \
        -f containers/admin-api/Dockerfile \
        --build-arg GIT_SHA=${GIT_SHA} \
        .
    SAVE IMAGE botwork/admin-api:local

admin-ui-image:
    ARG GIT_SHA=""
    FROM DOCKERFILE --platform=linux/amd64 \
        -f containers/admin-ui/Dockerfile \
        --build-arg GIT_SHA=${GIT_SHA} \
        .
    SAVE IMAGE botwork/admin-ui:local

session-broker-image:
    ARG GIT_SHA=""
    FROM DOCKERFILE --platform=linux/amd64 \
        -f containers/session-broker/Dockerfile \
        --build-arg GIT_SHA=${GIT_SHA} \
        .
    SAVE IMAGE botwork/session-broker:local

config-broker-image:
    ARG GIT_SHA=""
    FROM DOCKERFILE --platform=linux/amd64 \
        -f containers/config-broker/Dockerfile \
        --build-arg GIT_SHA=${GIT_SHA} \
        .
    SAVE IMAGE botwork/config-broker:local

control-plane-image:
    ARG GIT_SHA=""
    FROM DOCKERFILE --platform=linux/amd64 \
        -f containers/control-plane/Dockerfile \
        --build-arg GIT_SHA=${GIT_SHA} \
        .
    SAVE IMAGE botwork/control-plane:local

db-migrate-image:
    ARG GIT_SHA=""
    FROM DOCKERFILE --platform=linux/amd64 \
        -f containers/db-migrate/Dockerfile \
        --build-arg GIT_SHA=${GIT_SHA} \
        .
    SAVE IMAGE botwork/db-migrate:local

bootstrap-image:
    ARG GIT_SHA=""
    FROM DOCKERFILE --platform=linux/amd64 \
        -f containers/bootstrap/Dockerfile \
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
