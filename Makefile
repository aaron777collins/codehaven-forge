# Codehaven tasks. Builds and tests run in a Docker builder (see ARCHITECTURE.md section 15):
# on this machine by default, or on another host over ssh when CH_HOST and CH_REMOTE_ROOT are set
# (in the environment, or in the untracked deploy/dev/local.env; see deploy/dev/local.env.example).
# Nothing here ever picks a host for you. Local builds need a .env (copy .env.example).
#
#   make test [P=ch-core]    cargo nextest, whole workspace or one crate
#   make lint                clippy, warnings denied
#   make fmt                 formatting check
#   make ci                  the gate every stream runs before a merge (CH_NATIVE=1: already in a builder)
#   make sqlx                refresh .sqlx/ after a query or migration change
#   make docker-test         the runner's executor tests, against a throwaway Docker daemon
#   make e2e E2E_TAG=test-x  end-to-end run in a throwaway compose project, tag test-x
#   make release TAG=test-x  build the images under a tag (add DEPLOY=1 to replace a tag in use)
#   make up                  build images and start the instance the .env describes
#   make down                stop that instance (keeps data)
#   make clean               remove what Codehaven's builds made
#   make disk                disk usage on the build host
STREAM ?= foundation
P ?=
STEPS ?= 0-1
E2E_TAG ?=
TAG ?=
DEPLOY ?=

# CH_HOST and CH_REMOTE_ROOT may come from deploy/dev/local.env in the main checkout.
MAIN := $(shell git rev-parse --path-format=absolute --git-common-dir 2>/dev/null | sed 's,/\.git$$,,')
-include $(MAIN)/deploy/dev/local.env

ifneq ($(strip $(CH_NATIVE)),)
# Already inside a builder (Codehaven's own CI job, .codehaven/workflows/ci.yml): run here, with
# no Docker and no ssh.
R := bash -c
H := bash -c
HS := bash -c
else ifeq ($(strip $(CH_HOST)),)
R := bash deploy/dev/local.sh $(STREAM)
H := bash deploy/dev/local.sh --host $(STREAM)
HS := bash deploy/dev/local.sh --host src
else
R := bash deploy/dev/remote.sh $(STREAM)
H := bash deploy/dev/host.sh $(STREAM)
HS := bash deploy/dev/host.sh src
endif

.PHONY: test lint fmt ci sqlx sqlx-check e2e up down clean disk release docker-test portable

test:
	$(R) 'cargo nextest run $(if $(P),-p $(P),--workspace)'

lint:
	$(R) 'cargo xtask lint $(if $(P),-p $(P))'

fmt:
	$(R) 'cargo xtask fmt'

ci:
	$(R) 'cargo xtask ci'

portable:
	python3 deploy/check-portable.py

sqlx:
	CH_FETCH=.sqlx $(R) 'cargo xtask db-prepare'

sqlx-check:
	$(R) 'cargo xtask db-check'

release:
	@test -n "$(TAG)" || { echo "make release TAG=<tag>: the tag is required" >&2; exit 2; }
	$(H) 'CH_STREAM=$(STREAM) bash deploy/dev/release.sh $(TAG) $(if $(DEPLOY),--deploy)'

docker-test:
	$(H) 'CH_STREAM=$(STREAM) bash deploy/dev/docker-tests.sh'

e2e:
	@test -n "$(E2E_TAG)" || { echo "make e2e E2E_TAG=test-<word>: a throwaway tag is required" >&2; exit 2; }
	$(H) 'CH_STREAM=$(STREAM) bash deploy/dev/e2e.sh --tag $(E2E_TAG) --steps $(STEPS)'

up:
	$(HS) 'bash deploy/prod/up.sh'

down:
	$(HS) 'bash deploy/compose.sh down'

clean:
	$(HS) 'bash deploy/dev/clean.sh'

disk:
	$(HS) 'bash deploy/dev/diskcheck.sh'
