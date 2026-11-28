import ghidra.app.script.GhidraScript;
import ghidra.app.decompiler.*;
import ghidra.program.model.listing.*;
import java.io.*;
public class DecompAll extends GhidraScript {
  public void run() throws Exception {
    String out = getScriptArgs()[0];
    DecompInterface d = new DecompInterface();
    d.openProgram(currentProgram);
    PrintWriter pw = new PrintWriter(new FileWriter(out));
    for (Function f : currentProgram.getFunctionManager().getFunctions(true)) {
      if (f.isExternal() || f.isThunk()) continue;
      DecompileResults r = d.decompileFunction(f, 120, monitor);
      pw.println("// ==== " + f.getName() + " @ " + f.getEntryPoint());
      if (r != null && r.decompileCompleted()) pw.println(r.getDecompiledFunction().getC());
      else pw.println("// decompile failed");
    }
    pw.close();
  }
}
