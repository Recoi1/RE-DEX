// Disassembles and creates a function at each seed ("0xADDR arm|thumb" per line, # comments) that is not
// already inside one, then re-runs auto-analysis on the changes. Run before ExportFunctions.java.
// Usage: -postScript SeedFunctions.java <seed file>
import ghidra.app.script.GhidraScript;
import ghidra.program.model.address.*;
import ghidra.program.model.lang.Register;
import ghidra.program.model.listing.ContextChangeException;
import java.io.*;
import java.math.BigInteger;
import java.nio.file.*;

public class SeedFunctions extends GhidraScript {
    @Override
    public void run() throws Exception {
        Register tmode = currentProgram.getLanguage().getRegister("TMode");
        int made = 0, skipped = 0, failed = 0;
        for (String line : Files.readAllLines(Paths.get(getScriptArgs()[0]))) {
            line = line.trim();
            if (line.isEmpty() || line.startsWith("#")) continue;
            String[] p = line.split("\s+");
            Address a = toAddr(Long.decode(p[0]));
            if (getFunctionContaining(a) != null) { skipped++; continue; }
            if (getInstructionAt(a) == null) {
                // Inside an existing instruction (or data Ghidra already decoded otherwise): not a start.
                if (getInstructionContaining(a) != null) { failed++; continue; }
                BigInteger t = p[1].equals("thumb") ? BigInteger.ONE : BigInteger.ZERO;
                try {
                    currentProgram.getProgramContext().setValue(tmode, a, a, t);
                } catch (ContextChangeException e) { failed++; continue; }
                disassemble(a);
            }
            if (createFunction(a, null) != null) made++; else failed++;
        }
        analyzeChanges(currentProgram);
        println("SEED made=" + made + " skipped=" + skipped + " failed=" + failed);
    }
}
