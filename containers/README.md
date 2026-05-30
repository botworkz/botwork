# Containers

`botwork` builds one container image:

- `session-broker`: Rust session broker service image.

## Build locally

Build the image locally with:

```bash
make -C containers containers
```

This produces `botwork/session-broker:local`.

## Produce tarballs

Downstream consumers can export the locally built image as a tarball with:

```bash
make -C containers tarballs
```

That writes `containers/dist/session-broker.tar`, which consumers can load with `docker load`.
