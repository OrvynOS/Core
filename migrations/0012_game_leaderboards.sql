CREATE TABLE "game_saves" (
	"game_id" text NOT NULL,
	"account_id" text NOT NULL,
	"data" jsonb NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "game_saves_game_id_account_id_pk" PRIMARY KEY("game_id","account_id")
);
--> statement-breakpoint
CREATE TABLE "game_scores" (
	"game_id" text NOT NULL,
	"account_id" text NOT NULL,
	"value" double precision NOT NULL,
	"version" integer NOT NULL,
	"frames" integer NOT NULL,
	"runs" integer DEFAULT 1 NOT NULL,
	"created_at" timestamp with time zone DEFAULT now() NOT NULL,
	"updated_at" timestamp with time zone DEFAULT now() NOT NULL,
	CONSTRAINT "game_scores_game_id_account_id_pk" PRIMARY KEY("game_id","account_id")
);
--> statement-breakpoint
CREATE INDEX "game_scores_board_idx" ON "game_scores" USING btree ("game_id","value");