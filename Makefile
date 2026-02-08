.PHONY: help test run app goose conn create delete list reboot setup start stop

HOST ?=

CACHE := -v rlm-cargo-registry:/usr/local/cargo/registry \
		 -v rlm-cargo-git:/usr/local/cargo/git \
		 -v rlm-target:/work/target

define RLM
sudo docker run --runtime=runsc --rm $(CACHE) -v "$(CURDIR)":/work -w /work rust:latest
endef

define APP
sudo docker run --runtime=runsc --rm -p 3000:3000 --env-file .env --env APP_PORT=3000 \
	$(CACHE) -v "$(CURDIR)":/work -w /work rust:latest
endef

help:
	@echo "test       gVisor cargo test"
	@echo "run        gVisor cargo run"
	@echo "app        gVisor app on :3000"
	@echo "goose      run goose (HOST=...)"
	@echo "conn       connect to instance"
	@echo "create     create instance"
	@echo "delete     delete instance"
	@echo "list       list instances"
	@echo "reboot     reboot instance"
	@echo "setup      setup instance"
	@echo "start      start instance"
	@echo "stop       stop instance"

test:
	$(RLM) cargo test

run:
	$(RLM) cargo run

app:
	$(APP) cargo run -p app

goose:
	@if [ -z "$(HOST)" ]; then echo "HOST required"; exit 1; fi
	cargo run -p app --bin goose -- \
		--host "$(HOST)" \
		--timeout 86400 \
		--users 20 --run-time 1m \
		--report-file logs/report-$(shell date +"%Y-%m-%d_%H-%M-%S").html \
		--request-log logs/request-$(shell date +"%Y-%m-%d_%H-%M-%S").json

conn:
	bash scripts/conn.sh

create:
	bash scripts/create.sh

delete:
	bash scripts/delete.sh

list:
	bash scripts/list.sh

reboot:
	bash scripts/reboot.sh

setup:
	bash scripts/setup.sh

start:
	bash scripts/start.sh

stop:
	bash scripts/stop.sh
