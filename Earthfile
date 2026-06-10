VERSION 0.8

session-broker-image:
    ARG GIT_SHA=""
    FROM DOCKERFILE --platform=linux/amd64 \
        -f containers/session-broker/Dockerfile \
        --build-arg GIT_SHA=${GIT_SHA} \
        .
    SAVE IMAGE botwork/session-broker:local

images:
    BUILD +session-broker-image
