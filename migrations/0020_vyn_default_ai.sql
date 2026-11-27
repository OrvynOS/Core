ALTER TABLE "inference_keys" ADD COLUMN "purpose" text;
--> statement-breakpoint
CREATE TABLE "vyn_allowance" (
	"account_id" text NOT NULL,
	"month" text NOT NULL,
	"used_micro" bigint DEFAULT 0 NOT NULL,
	CONSTRAINT "vyn_allowance_pk" PRIMARY KEY("account_id","month")
);
