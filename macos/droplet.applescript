-- Sunscreen.app: drop MP3/WAV files on the icon. Outputs go to a "sunscreen"
-- folder next to each file, using the recommended flags.
on open theFiles
	set bin to quoted form of (POSIX path of (path to resource "sunscreen"))
	repeat with f in theFiles
		set p to POSIX path of f
		set outDir to (do shell script "dirname " & quoted form of p) & "/sunscreen"
		try
			do shell script bin & " " & quoted form of p & " " & quoted form of outDir & " --smooth --widen --excite"
		on error msg
			display alert "sunscreen failed on " & p message msg
		end try
	end repeat
	display notification "Finished " & (count of theFiles) & " file(s)" with title "sunscreen"
	do shell script "open " & quoted form of outDir
end open

on run
	display dialog "Drop MP3 or WAV files onto the Sunscreen icon." buttons {"OK"} default button 1 with title "sunscreen"
end run
