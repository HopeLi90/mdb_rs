/**
 * 生成一份真实的 ESRI Personal Geodatabase（*.mdb，Jet 4）样本库，用于
 * pgdb-rs 的 ODBC / GDAL 交叉验证。
 *
 * <p>为什么用 Java + Jackcess 而不是 GDAL：GDAL 的 PGeo 驱动是只读的
 * （ogrinfo --formats 显示 "PGeo -vector- (ro)"），Access .mdb 属于专有格式，
 * GDAL 无法创建。Jackcess 可以原生写 Jet 4，因此这里由它造库，
 * 再由 GDAL（ogrinfo/ogr2ogr）只读读取，做双向一致性交叉验证。
 *
 * <p>用法：
 * <pre>
 *   javac -cp "jackcess-4.0.5.jar:commons-lang3-3.12.0.jar:commons-logging-1.2.jar" MakeMdb.java
 *   java  -cp ".:jackcess-4.0.5.jar:commons-lang3-3.12.0.jar:commons-logging-1.2.jar" \
 *         MakeMdb out.mdb --model=legacy|items --shape-column=ole|binary
 * </pre>
 *
 * <p>产出的数据组织（两种元数据模型共用同一套业务表）：
 * <pre>
 *   Roads       独立要素类（Polyline）
 *   Sensors     独立要素类（Point，含一个 PointZ）
 *   Meters      独立要素类（Multipoint）
 *   OwnerTable  独立数据表（无几何）
 *   Hydrology\  要素数据集
 *     Ponds     数据集内要素类（Polygon）
 * </pre>
 */
import com.healthmarketscience.jackcess.Column;
import com.healthmarketscience.jackcess.ColumnBuilder;
import com.healthmarketscience.jackcess.DataType;
import com.healthmarketscience.jackcess.Database;
import com.healthmarketscience.jackcess.DatabaseBuilder;
import com.healthmarketscience.jackcess.Table;
import com.healthmarketscience.jackcess.TableBuilder;

import java.io.File;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.sql.Timestamp;
import java.sql.Types;

public final class MakeMdb {

    /** 空间索引一级网格尺寸（ArcGIS 默认推荐值） */
    private static final double GRID = 420.0;

    private static final String T_FC = "{70737809-852C-4A03-9E22-2CECEA5B9BFA}";
    private static final String T_FD = "{A06F9B4B-8C0B-4C4B-9F0D-2F5D9DC0A0F1}";
    private static final String T_TB = "{CD06C6A0-7C63-4C0B-9E63-3F4F8B3A1C2D}";
    private static final String T_RL = "{1C6A0F0A-4D9E-4C5E-9F63-9B0F6E1B2A31}";

    public static void main(String[] args) throws Exception {
        String out = "sample.mdb";
        String model = "legacy";
        boolean ole = true;
        for (String a : args) {
            if (a.startsWith("--model=")) {
                model = a.substring("--model=".length());
            } else if (a.startsWith("--shape-column=")) {
                ole = !"binary".equalsIgnoreCase(a.substring("--shape-column=".length()));
            } else {
                out = a;
            }
        }
        File f = new File(out);
        if (f.exists() && !f.delete()) {
            throw new IllegalStateException("无法删除旧文件: " + out);
        }
        try (Database db = DatabaseBuilder.create(Database.FileFormat.V2000, f)) {
            buildBusiness(db, ole);
            if ("items".equalsIgnoreCase(model)) {
                buildItemsModel(db);
            } else {
                buildLegacyModel(db);
            }
        }
        System.out.println("已生成 " + f.getAbsolutePath()
                + " [model=" + model + ", shape=" + (ole ? "OLE" : "BINARY") + "]");
    }

    // ============================================================ 业务表

    private static void buildBusiness(Database db, boolean ole) throws Exception {
        int blob = ole ? Types.LONGVARBINARY : Types.BINARY;

        new TableBuilder("Roads")
                .addColumn(new ColumnBuilder("OBJECTID", DataType.LONG).setAutoNumber(true))
                .addColumn(new ColumnBuilder("NAME").setSQLType(Types.VARCHAR).setLengthInUnits(50))
                .addColumn(new ColumnBuilder("CLASS", DataType.LONG))
                .addColumn(new ColumnBuilder("Shape_Length", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("Shape").setSQLType(blob))
                .toTable(db);

        new TableBuilder("Sensors")
                .addColumn(new ColumnBuilder("OBJECTID", DataType.LONG).setAutoNumber(true))
                .addColumn(new ColumnBuilder("NAME").setSQLType(Types.VARCHAR).setLengthInUnits(50))
                .addColumn(new ColumnBuilder("VALUE_", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("Shape").setSQLType(blob))
                .toTable(db);

        new TableBuilder("Meters")
                .addColumn(new ColumnBuilder("OBJECTID", DataType.LONG).setAutoNumber(true))
                .addColumn(new ColumnBuilder("NAME").setSQLType(Types.VARCHAR).setLengthInUnits(50))
                .addColumn(new ColumnBuilder("Shape").setSQLType(blob))
                .toTable(db);

        new TableBuilder("Ponds")
                .addColumn(new ColumnBuilder("OBJECTID", DataType.LONG).setAutoNumber(true))
                .addColumn(new ColumnBuilder("NAME").setSQLType(Types.VARCHAR).setLengthInUnits(50))
                .addColumn(new ColumnBuilder("DEPTH", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("Shape_Length", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("Shape_Area", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("Shape").setSQLType(blob))
                .toTable(db);

        new TableBuilder("OwnerTable")
                .addColumn(new ColumnBuilder("OBJECTID", DataType.LONG).setAutoNumber(true))
                .addColumn(new ColumnBuilder("OWNER").setSQLType(Types.VARCHAR).setLengthInUnits(40))
                .addColumn(new ColumnBuilder("AREA", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("UPDATED").setSQLType(Types.TIMESTAMP))
                .toTable(db);

        // ---- Roads：独立线要素类
        Table roads = db.getTable("Roads");
        roads.addRow(Column.AUTO_NUMBER, "人民路", 2, 20.0,
                polyline(new double[][]{{0, 0}, {10, 0}, {10, 10}}));
        roads.addRow(Column.AUTO_NUMBER, "建国路", 1, 10.0,
                polyline(new double[][]{{20, 5}, {30, 5}}));
        shapeIndex(db, "Roads", new double[][]{
                {0, 0, 10, 10},
                {20, 5, 30, 5},
        });

        // ---- Sensors：独立点要素类（第 3 个为 PointZ）
        Table sensors = db.getTable("Sensors");
        sensors.addRow(Column.AUTO_NUMBER, "S-01", 12.5, point(5, 5, false));
        sensors.addRow(Column.AUTO_NUMBER, "S-02", 18.0, point(15, 25, false));
        sensors.addRow(Column.AUTO_NUMBER, "S-03", 7.25, point(1, 1, true));
        shapeIndex(db, "Sensors", new double[][]{
                {5, 5, 5, 5},
                {15, 25, 15, 25},
                {1, 1, 1, 1},
        });

        // ---- Meters：独立多点要素类
        Table meters = db.getTable("Meters");
        meters.addRow(Column.AUTO_NUMBER, "M-1",
                multipoint(new double[][]{{0, 0}, {1, 1}, {2, 3}}));
        shapeIndex(db, "Meters", new double[][]{{0, 0, 2, 3}});

        // ---- Ponds：要素数据集 Hydrology 内的面要素类
        Table ponds = db.getTable("Ponds");
        ponds.addRow(Column.AUTO_NUMBER, "南湖", 3.5, 40.0, 100.0,
                polygon(new double[][]{{0, 0}, {0, 10}, {10, 10}, {10, 0}, {0, 0}}));
        ponds.addRow(Column.AUTO_NUMBER, "西湖", 2.0, 24.0, 36.0,
                polygon(new double[][]{{20, 20}, {20, 26}, {26, 26}, {26, 20}, {20, 20}}));
        shapeIndex(db, "Ponds", new double[][]{
                {0, 0, 10, 10},
                {20, 20, 26, 26},
        });

        // ---- OwnerTable：独立数据表（无几何）
        Table owner = db.getTable("OwnerTable");
        owner.addRow(Column.AUTO_NUMBER, "张三", 512.5,
                Timestamp.valueOf("2024-01-01 10:00:00"));
        owner.addRow(Column.AUTO_NUMBER, "李四", 340.25,
                Timestamp.valueOf("2024-02-15 09:30:00"));
    }

    /** 建 `<表>_SHAPE_Index`（ArcGIS 定位要素所依赖的空间索引表） */
    private static void shapeIndex(Database db, String table, double[][] boxes) throws Exception {
        String name = table + "_SHAPE_Index";
        new TableBuilder(name)
                .addColumn(new ColumnBuilder("IndexedObjectId", DataType.LONG))
                .addColumn(new ColumnBuilder("MinGX", DataType.LONG))
                .addColumn(new ColumnBuilder("MinGY", DataType.LONG))
                .addColumn(new ColumnBuilder("MaxGX", DataType.LONG))
                .addColumn(new ColumnBuilder("MaxGY", DataType.LONG))
                .toTable(db);
        Table t = db.getTable(name);
        for (int i = 0; i < boxes.length; i++) {
            double[] b = boxes[i];
            t.addRow(i + 1,
                    (long) Math.floor(b[0] / GRID),
                    (long) Math.floor(b[1] / GRID),
                    (long) Math.ceil(b[2] / GRID),
                    (long) Math.ceil(b[3] / GRID));
        }
    }

    // ============================================ 元数据模型一：Legacy（8.x/9.0）

    private static void buildLegacyModel(Database db) throws Exception {
        new TableBuilder("GDB_ObjectClasses")
                .addColumn(new ColumnBuilder("ID", DataType.LONG))
                .addColumn(new ColumnBuilder("Name").setSQLType(Types.VARCHAR).setLengthInUnits(128))
                .addColumn(new ColumnBuilder("DatasetType", DataType.LONG))
                .addColumn(new ColumnBuilder("DatasetID", DataType.LONG))
                .toTable(db);

        new TableBuilder("GDB_FeatureClasses")
                .addColumn(new ColumnBuilder("ObjectClassID", DataType.LONG))
                .addColumn(new ColumnBuilder("FeatureType", DataType.LONG))
                .addColumn(new ColumnBuilder("GeometryType", DataType.LONG))
                .addColumn(new ColumnBuilder("ShapeFieldName").setSQLType(Types.VARCHAR).setLengthInUnits(32))
                .toTable(db);

        new TableBuilder("GDB_FeatureDataset")
                .addColumn(new ColumnBuilder("ID", DataType.LONG))
                .addColumn(new ColumnBuilder("Name").setSQLType(Types.VARCHAR).setLengthInUnits(128))
                .addColumn(new ColumnBuilder("SRID", DataType.LONG))
                .toTable(db);

        new TableBuilder("GDB_GeomColumns")
                .addColumn(new ColumnBuilder("TableName").setSQLType(Types.VARCHAR).setLengthInUnits(128))
                .addColumn(new ColumnBuilder("FieldName").setSQLType(Types.VARCHAR).setLengthInUnits(32))
                .addColumn(new ColumnBuilder("ShapeType", DataType.LONG))
                .addColumn(new ColumnBuilder("ExtentLeft", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("ExtentBottom", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("ExtentRight", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("ExtentTop", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("IdxOriginX", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("IdxOriginY", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("IdxGridSize", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("SRID", DataType.LONG))
                // GDAL PGeo 驱动强制要求这两列
                .addColumn(new ColumnBuilder("HasZ", DataType.LONG))
                .addColumn(new ColumnBuilder("HasM", DataType.LONG))
                .toTable(db);

        new TableBuilder("GDB_SpatialRefs")
                .addColumn(new ColumnBuilder("SRID", DataType.LONG))
                .addColumn(new ColumnBuilder("FalseX", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("FalseY", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("XYUnits", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("FalseZ", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("ZUnits", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("FalseM", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("MUnits", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("XYClusterTol", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("SRTEXT").setSQLType(Types.LONGVARCHAR))
                .toTable(db);

        new TableBuilder("GDB_FieldInfo")
                .addColumn(new ColumnBuilder("TableName").setSQLType(Types.VARCHAR).setLengthInUnits(128))
                .addColumn(new ColumnBuilder("FieldName").setSQLType(Types.VARCHAR).setLengthInUnits(64))
                .addColumn(new ColumnBuilder("AliasName").setSQLType(Types.VARCHAR).setLengthInUnits(64))
                .toTable(db);

        Table oc = db.getTable("GDB_ObjectClasses");
        oc.addRow(1, "Roads", 3, null);
        oc.addRow(2, "OwnerTable", 1, null);
        oc.addRow(3, "Ponds", 3, 1);
        oc.addRow(4, "Sensors", 3, null);
        oc.addRow(5, "Meters", 3, null);

        Table fc = db.getTable("GDB_FeatureClasses");
        fc.addRow(1, 1, 3, "Shape");   // Roads   PolyLine   (esriGeometryPolyline = 3)
        fc.addRow(3, 1, 4, "Shape");   // Ponds   Polygon     (esriGeometryPolygon = 4)
        fc.addRow(4, 1, 1, "Shape");   // Sensors Point       (esriGeometryPoint = 1)
        fc.addRow(5, 1, 2, "Shape");   // Meters  MultiPoint  (esriGeometryMultipoint = 2)

        Table fd = db.getTable("GDB_FeatureDataset");
        fd.addRow(1, "Hydrology", 1);

        Table gc = db.getTable("GDB_GeomColumns");
        gc.addRow("Roads", "Shape", 3, 0.0, 0.0, 30.0, 10.0, 0.0, 0.0, GRID, 1, 0, 0);
        gc.addRow("Ponds", "Shape", 4, 0.0, 0.0, 26.0, 26.0, 0.0, 0.0, GRID, 1, 0, 0);
        gc.addRow("Sensors", "Shape", 1, 1.0, 1.0, 15.0, 25.0, 0.0, 0.0, GRID, 1, 1, 0);
        gc.addRow("Meters", "Shape", 2, 0.0, 0.0, 2.0, 3.0, 0.0, 0.0, GRID, 1, 0, 0);

        Table sr = db.getTable("GDB_SpatialRefs");
        sr.addRow(1, -400.0, -400.0, 100000.0, -1000.0, 100000.0, -1000.0, 100000.0, 0.001,
                "GEOGCS[\"GCS_WGS_1984\",DATUM[\"D_WGS_1984\","
                        + "SPHEROID[\"WGS_1984\",6378137.0,298.257223563]],"
                        + "PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]]");

        Table fi = db.getTable("GDB_FieldInfo");
        fi.addRow("Roads", "NAME", "道路名称");
        fi.addRow("Roads", "CLASS", "道路等级");
        fi.addRow("OwnerTable", "OWNER", "权利人");
        fi.addRow("Ponds", "DEPTH", "水深");
    }

    // ============================================ 元数据模型二：Items（9.2+）

    private static void buildItemsModel(Database db) throws Exception {
        final int guid = Types.VARCHAR;
        final int len = 38;

        new TableBuilder("GDB_ItemTypes")
                .addColumn(new ColumnBuilder("UUID").setSQLType(guid).setLengthInUnits(len))
                .addColumn(new ColumnBuilder("Name").setSQLType(Types.VARCHAR).setLengthInUnits(64))
                .toTable(db);

        new TableBuilder("GDB_ItemRelationshipTypes")
                .addColumn(new ColumnBuilder("UUID").setSQLType(guid).setLengthInUnits(len))
                .addColumn(new ColumnBuilder("Name").setSQLType(Types.VARCHAR).setLengthInUnits(64))
                .toTable(db);

        new TableBuilder("GDB_Items")
                .addColumn(new ColumnBuilder("UUID").setSQLType(guid).setLengthInUnits(len))
                .addColumn(new ColumnBuilder("Type").setSQLType(guid).setLengthInUnits(len))
                .addColumn(new ColumnBuilder("Name").setSQLType(Types.VARCHAR).setLengthInUnits(128))
                .addColumn(new ColumnBuilder("PhysicalName").setSQLType(Types.VARCHAR).setLengthInUnits(128))
                .addColumn(new ColumnBuilder("DatasetInfo1").setSQLType(Types.VARCHAR).setLengthInUnits(64))
                .addColumn(new ColumnBuilder("DatasetSubtype1", DataType.LONG))
                .addColumn(new ColumnBuilder("DatasetSubtype2", DataType.LONG))
                .addColumn(new ColumnBuilder("Definition").setSQLType(Types.LONGVARCHAR))
                .toTable(db);

        new TableBuilder("GDB_ItemRelationships")
                .addColumn(new ColumnBuilder("UUID").setSQLType(guid).setLengthInUnits(len))
                .addColumn(new ColumnBuilder("Type").setSQLType(guid).setLengthInUnits(len))
                .addColumn(new ColumnBuilder("OriginID").setSQLType(guid).setLengthInUnits(len))
                .addColumn(new ColumnBuilder("DestID").setSQLType(guid).setLengthInUnits(len))
                .toTable(db);

        // 空间参考（Items 模型同样使用 GDB_SpatialRefs / GDB_GeomColumns 做容错）
        new TableBuilder("GDB_SpatialRefs")
                .addColumn(new ColumnBuilder("SRID", DataType.LONG))
                .addColumn(new ColumnBuilder("FalseX", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("FalseY", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("XYUnits", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("XYClusterTol", DataType.DOUBLE))
                .addColumn(new ColumnBuilder("SRTEXT").setSQLType(Types.LONGVARCHAR))
                .toTable(db);
        Table sr = db.getTable("GDB_SpatialRefs");
        sr.addRow(1, -400.0, -400.0, 100000.0, 0.001,
                "GEOGCS[\"GCS_WGS_1984\",DATUM[\"D_WGS_1984\","
                        + "SPHEROID[\"WGS_1984\",6378137.0,298.257223563]],"
                        + "PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]]");

        Table types = db.getTable("GDB_ItemTypes");
        types.addRow(T_FC, "Feature Class");
        types.addRow(T_FD, "Feature Dataset");
        types.addRow(T_TB, "Table");

        Table relTypes = db.getTable("GDB_ItemRelationshipTypes");
        relTypes.addRow(T_RL, "FeatureDataset");

        String uRoads = "{11111111-1111-1111-1111-111111111111}";
        String uPonds = "{22222222-2222-2222-2222-222222222222}";
        String uOwner = "{33333333-3333-3333-3333-333333333333}";
        String uSens = "{44444444-4444-4444-4444-444444444444}";
        String uMete = "{55555555-5555-5555-5555-555555555555}";
        String uHydr = "{66666666-6666-6666-6666-666666666666}";

        Table items = db.getTable("GDB_Items");
        items.addRow(uRoads, T_FC, "Roads", "Roads", "Shape", 1, 3,
                "<DEFeatureClassInfo><ShapeFieldName>Shape</ShapeFieldName></DEFeatureClassInfo>");
        items.addRow(uOwner, T_TB, "OwnerTable", "OwnerTable", null, null, null, null);
        items.addRow(uHydr, T_FD, "Hydrology", null, null, null, null, null);
        items.addRow(uPonds, T_FC, "Ponds", "Ponds", "Shape", 1, 4,
                "<DEFeatureClassInfo><ShapeFieldName>Shape</ShapeFieldName></DEFeatureClassInfo>");
        items.addRow(uSens, T_FC, "Sensors", "Sensors", "Shape", 1, 1, null);
        items.addRow(uMete, T_FC, "Meters", "Meters", "Shape", 1, 2, null);

        Table rels = db.getTable("GDB_ItemRelationships");
        // Dest 端为要素数据集 → Ponds 归属于 Hydrology
        rels.addRow("{77777777-7777-7777-7777-777777777777}", T_RL, uPonds, uHydr);
    }

    // ============================================================ Shape 编码

    private static ByteBuffer le(int cap) {
        return ByteBuffer.allocate(cap).order(ByteOrder.LITTLE_ENDIAN);
    }

    /** Point（1）；withZ 时输出 PointZ（11）= X,Y,Z,M */
    private static byte[] point(double x, double y, boolean withZ) {
        if (withZ) {
            ByteBuffer b = le(4 + 8 * 4);
            b.putInt(11).putDouble(x).putDouble(y).putDouble(5.0).putDouble(0.0);
            return b.array();
        }
        ByteBuffer b = le(4 + 16);
        b.putInt(1).putDouble(x).putDouble(y);
        return b.array();
    }

    /** PolyLine（3）：单部件 */
    private static byte[] polyline(double[][] pts) {
        return multipart(3, new double[][][]{pts});
    }

    /** Polygon（5）：单环 */
    private static byte[] polygon(double[][] pts) {
        return multipart(5, new double[][][]{pts});
    }

    /** MultiPoint（8）：Box + NumPoints + Points[]（没有 NumParts） */
    private static byte[] multipoint(double[][] pts) {
        double[] box = boxOf(pts);
        ByteBuffer b = le(4 + 32 + 4 + pts.length * 16);
        b.putInt(8);
        for (double v : box) {
            b.putDouble(v);
        }
        b.putInt(pts.length);
        for (double[] p : pts) {
            b.putDouble(p[0]).putDouble(p[1]);
        }
        return b.array();
    }

    private static byte[] multipart(int type, double[][][] parts) {
        int n = 0;
        for (double[][] p : parts) {
            n += p.length;
        }
        double[] box = boxOf(flatten(parts));
        ByteBuffer b = le(4 + 32 + 4 + 4 + parts.length * 4 + n * 16);
        b.putInt(type);
        for (double v : box) {
            b.putDouble(v);
        }
        b.putInt(parts.length);
        b.putInt(n);
        int acc = 0;
        for (double[][] p : parts) {
            b.putInt(acc);
            acc += p.length;
        }
        for (double[][] p : parts) {
            for (double[] xy : p) {
                b.putDouble(xy[0]).putDouble(xy[1]);
            }
        }
        return b.array();
    }

    private static double[][] flatten(double[][][] parts) {
        int n = 0;
        for (double[][] p : parts) {
            n += p.length;
        }
        double[][] out = new double[n][];
        int i = 0;
        for (double[][] p : parts) {
            for (double[] xy : p) {
                out[i++] = xy;
            }
        }
        return out;
    }

    /** 返回 {minX, minY, maxX, maxY} */
    private static double[] boxOf(double[][] pts) {
        double[] b = {Double.MAX_VALUE, Double.MAX_VALUE, -Double.MAX_VALUE, -Double.MAX_VALUE};
        for (double[] p : pts) {
            b[0] = Math.min(b[0], p[0]);
            b[1] = Math.min(b[1], p[1]);
            b[2] = Math.max(b[2], p[0]);
            b[3] = Math.max(b[3], p[1]);
        }
        return b;
    }

    private MakeMdb() {
    }
}
