CREATE TABLE "price_observations" (
	"feed" text NOT NULL,
	"round" integer NOT NULL,
	"agent_id" text NOT NULL,
	"price" text NOT NULL,
	"signature" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "price_observations_feed_round_agent_id_pk" PRIMARY KEY("feed","round","agent_id")
);
--> statement-breakpoint
CREATE TABLE "price_report_signatures" (
	"feed" text NOT NULL,
	"round" integer NOT NULL,
	"agent_id" text NOT NULL,
	"signature" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "price_report_signatures_feed_round_agent_id_pk" PRIMARY KEY("feed","round","agent_id")
);
--> statement-breakpoint
CREATE TABLE "price_reports" (
	"feed" text NOT NULL,
	"round" integer NOT NULL,
	"price" text NOT NULL,
	"observations" integer NOT NULL,
	"participants" jsonb,
	"aggregate_signature" text,
	"submit_tx" text,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "price_reports_feed_round_pk" PRIMARY KEY("feed","round")
);
