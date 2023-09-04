# Canary

![Docker Pulls](https://img.shields.io/docker/pulls/acje/canary)

Canary has only one feature. It replies "Canary [version] is alive!". And does some logging server side.

Rust/Actix-web 4

<https://hub.docker.com/r/acje/canary/>

Run:

podman run -e "PORT=8080" -p 8080:8080 acje/canary:latest

Response at <http://localhost:8080/>

On cloud run:
<https://canary-app-acje-qvw63kdulq-ue.a.run.app/>
