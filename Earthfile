VERSION 0.8

session-broker-image:
    FROM DOCKERFILE --platform=linux/amd64 -f containers/session-broker/Dockerfile .
    SAVE IMAGE botwork/session-broker:local

images:
    BUILD +session-broker-image
