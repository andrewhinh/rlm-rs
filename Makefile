.PHONY: help app goose conn create delete list reboot aws-setup ec2-setup start stop

ARG := $(word 2,$(MAKECMDGOALS))
HOST ?=

help:
	@echo "app        run app"
	@echo "goose      run goose against <host>"
	@echo "conn       connect to instance <instance-id>"
	@echo "create     create instance"
	@echo "delete     delete instance <instance-id>"
	@echo "list       list instances"
	@echo "reboot     reboot instance <instance-id>"
	@echo "aws-setup  setup aws account"
	@echo "ec2-setup  setup connected ec2 instance"
	@echo "start      start instance <instance-id>"
	@echo "stop       stop instance <instance-id>"

app:
	cargo build -p app --bin sandbox_worker
	cargo run -p app

goose:
	@if [ -z "$(HOST)" ]; then echo "usage: make goose HOST=<host>"; exit 1; fi
	mkdir -p logs
	cargo run -p app --bin goose -- \
		--host "$(HOST)" \
		--timeout 86400 \
		--users 20 --run-time 1m \
		--report-file logs/report-$(shell date +"%Y-%m-%d_%H-%M-%S").html \
		--request-log logs/request-$(shell date +"%Y-%m-%d_%H-%M-%S").json

conn:
	bash scripts/conn.sh $(ARG)

create:
	bash scripts/create.sh

delete:
	bash scripts/delete.sh $(ARG)

list:
	bash scripts/list.sh

reboot:
	bash scripts/reboot.sh $(ARG)

aws-setup:
	bash scripts/setup.sh

ec2-setup:
	bash scripts/ec2-setup.sh

start:
	bash scripts/start.sh $(ARG)

stop:
	bash scripts/stop.sh $(ARG)

$(ARG):
	@:
