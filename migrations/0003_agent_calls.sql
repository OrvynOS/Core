CREATE TABLE "agent_calls" (
	"id" serial PRIMARY KEY NOT NULL,
	"agent_id" text NOT NULL,
	"subject_kind" text NOT NULL,
	"subject_ref" text NOT NULL,
	"label" text NOT NULL,
	"direction" text NOT NULL,
	"confidence" double precision NOT NULL,
	"horizon_hours" integer NOT NULL,
	"thesis" text NOT NULL,
	"evidence" jsonb,
	"entry_price" double precision NOT NULL,
	"due_at" timestamp with time zone NOT NULL,
	"graded_at" timestamp with time zone,
	"exit_price" double precision,
	"move_pct" double precision,
	"outcome" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE INDEX "agent_calls_agent_idx" ON "agent_calls" USING btree ("agent_id","created_at");--> statement-breakpoint
CREATE INDEX "agent_calls_due_idx" ON "agent_calls" USING btree ("graded_at","due_at");