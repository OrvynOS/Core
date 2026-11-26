ALTER TABLE "rng_nodes" ADD COLUMN "chain_index" integer;--> statement-breakpoint
ALTER TABLE "rng_rounds" ADD COLUMN "commit_tx" text;--> statement-breakpoint
ALTER TABLE "rng_rounds" ADD COLUMN "fulfill_tx" text;