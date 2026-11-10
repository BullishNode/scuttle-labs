-- Stored mailbox destinations must remain paired with their round outputs.
ALTER TABLE round_part_output
	ADD COLUMN output_index INTEGER CHECK (output_index >= 0);
CREATE UNIQUE INDEX round_part_output_position
	ON round_part_output (participation_id, output_index)
	WHERE output_index IS NOT NULL;
