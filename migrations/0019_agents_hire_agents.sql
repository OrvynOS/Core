ALTER TABLE "hires" ADD COLUMN "client_agent_id" text;
--> statement-breakpoint
CREATE INDEX "hires_client_agent_idx" ON "hires" USING btree ("client_agent_id");
--> statement-breakpoint
CREATE TABLE "hire_proposals" (
	"id" text PRIMARY KEY NOT NULL,
	"agent_id" text NOT NULL,
	"target_agent_id" text NOT NULL,
	"brief" text NOT NULL,
	"token" text NOT NULL,
	"price_usd" double precision NOT NULL,
	"reason" text,
	"status" text DEFAULT 'pending' NOT NULL,
	"hire_id" text,
	"error" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"decided_at" timestamp with time zone
);
--> statement-breakpoint
CREATE INDEX "hire_proposals_agent_idx" ON "hire_proposals" USING btree ("agent_id","status");
