// Exports every function as "0xSTART 0xEND arm|thumb" (end exclusive), sorted, non-overlapping,
// clipped to the text segment. Usage: -postScript ExportFunctions.java <out file>
import ghidra.app.script.GhidraScript;
import ghidra.program.model.address.*;
import ghidra.program.model.lang.Register;
import ghidra.program.model.listing.*;
import java.io.*;
import java.math.BigInteger;
import java.util.*;

public class ExportFunctions extends GhidraScript {
    static final long TEXT_START = 0x00100000L, TEXT_END = 0x0053BEDCL;

    @Override
    public void run() throws Exception {
        String out = getScriptArgs()[0];
        Register tmode = currentProgram.getLanguage().getRegister("TMode");
        // BinaryLoader sets no entry point, so the _start stub at the image base is never seeded.
        Address base = toAddr(TEXT_START);
        if (getFunctionAt(base) == null) {
            disassemble(base);
            createFunction(base, null);
        }
        List<long[]> fs = new ArrayList<>(); // start, end, thumb
        int dropped = 0;
        for (Function f : currentProgram.getFunctionManager().getFunctions(true)) {
            Address ep = f.getEntryPoint();
            long start = ep.getOffset() & 0xFFFFFFFFL;
            long end = start + 1;
            for (AddressRange r : f.getBody().getAddressRanges()) {
                if (r.contains(ep)) end = r.getMaxAddress().getOffset() + 1;
            }
            if (start < TEXT_START || start >= TEXT_END) { dropped++; continue; }
            end = Math.min(end, TEXT_END);
            BigInteger v = currentProgram.getProgramContext().getValue(tmode, ep, false);
            fs.add(new long[] {start, end, (v != null && v.intValue() == 1) ? 1 : 0});
        }
        fs.sort((a, b) -> Long.compare(a[0], b[0]));
        int clipped = 0, zero = 0;
        List<long[]> res = new ArrayList<>();
        for (int i = 0; i < fs.size(); i++) {
            long[] f = fs.get(i);
            if (i + 1 < fs.size() && f[1] > fs.get(i + 1)[0]) { f[1] = fs.get(i + 1)[0]; clipped++; }
            if (f[1] <= f[0]) { zero++; continue; }
            res.add(f);
        }
        try (PrintWriter w = new PrintWriter(new FileWriter(out))) {
            w.println("# x-usa-v0.2.0 code.bin functions, exported by tools/ghidra/ExportFunctions.java from Ghidra 12.2 auto-analysis");
            w.println("# count: " + res.size());
            for (long[] f : res)
                w.printf("0x%08X 0x%08X %s%n", f[0], f[1], f[2] == 1 ? "thumb" : "arm");
        }
        println("EXPORT count=" + res.size() + " clipped=" + clipped + " dropped=" + dropped + " zero=" + zero);
    }
}
