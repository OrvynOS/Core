CREATE TABLE "rng_nodes" (
	"agent_id" text PRIMARY KEY NOT NULL,
	"public_key" text NOT NULL,
	"secret_encrypted" text,
	"external" boolean DEFAULT false NOT NULL,
	"active" boolean DEFAULT true NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE TABLE "rng_partials" (
	"round" integer NOT NULL,
	"agent_id" text NOT NULL,
	"signature" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "rng_partials_round_agent_id_pk" PRIMARY KEY("round","agent_id")
);
--> statement-breakpoint
CREATE TABLE "rng_rounds" (
	"round" integer PRIMARY KEY NOT NULL,
	"output" text NOT NULL,
	"aggregate_signature" text NOT NULL,
	"participants" jsonb NOT NULL,
	"drand_round" integer NOT NULL,
	"drand_signature" text NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL
);
--> statement-breakpoint
CREATE INDEX "rng_partials_agent_idx" ON "rng_partials" USING btree ("agent_id","round");