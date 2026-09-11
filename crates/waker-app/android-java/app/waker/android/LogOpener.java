package app.waker.android;

import android.app.Activity;
import android.content.ContentResolver;
import android.content.ContentUris;
import android.content.ContentValues;
import android.content.Intent;
import android.database.Cursor;
import android.net.Uri;
import android.os.Build;
import android.os.Environment;
import android.provider.BaseColumns;
import android.provider.MediaStore;

import java.io.IOException;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;

public final class LogOpener {
    private static final String DISPLAY_NAME = "waker-log.txt";
    private static final String RELATIVE_PATH = Environment.DIRECTORY_DOWNLOADS + "/Waker/";

    private LogOpener() {}

    public static void open(Activity activity, String text) throws IOException {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) {
            Intent send = new Intent(Intent.ACTION_SEND)
                    .setType("text/plain")
                    .putExtra(Intent.EXTRA_TEXT, text);
            activity.startActivity(Intent.createChooser(send, "Open Waker log"));
            return;
        }

        ContentResolver resolver = activity.getContentResolver();
        Uri collection = MediaStore.Downloads.EXTERNAL_CONTENT_URI;
        Uri uri = findExisting(resolver, collection);

        if (uri == null) {
            ContentValues values = new ContentValues();
            values.put(MediaStore.MediaColumns.DISPLAY_NAME, DISPLAY_NAME);
            values.put(MediaStore.MediaColumns.MIME_TYPE, "text/plain");
            values.put(MediaStore.MediaColumns.RELATIVE_PATH, RELATIVE_PATH);
            values.put(MediaStore.MediaColumns.IS_PENDING, 1);
            uri = resolver.insert(collection, values);
            if (uri == null) {
                throw new IOException("MediaStore refused to create the log file");
            }
        }

        try (OutputStream output = resolver.openOutputStream(uri, "wt")) {
            if (output == null) {
                throw new IOException("MediaStore did not provide a log output stream");
            }
            output.write(text.getBytes(StandardCharsets.UTF_8));
        }

        ContentValues ready = new ContentValues();
        ready.put(MediaStore.MediaColumns.IS_PENDING, 0);
        resolver.update(uri, ready, null, null);

        Intent view = new Intent(Intent.ACTION_VIEW)
                .setDataAndType(uri, "text/plain")
                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
        activity.startActivity(Intent.createChooser(view, "Open Waker log"));
    }

    private static Uri findExisting(ContentResolver resolver, Uri collection) {
        String[] projection = { BaseColumns._ID };
        String selection = MediaStore.MediaColumns.DISPLAY_NAME + "=? AND "
                + MediaStore.MediaColumns.RELATIVE_PATH + "=?";
        String[] args = { DISPLAY_NAME, RELATIVE_PATH };

        try (Cursor cursor = resolver.query(collection, projection, selection, args, null)) {
            if (cursor != null && cursor.moveToFirst()) {
                return ContentUris.withAppendedId(collection, cursor.getLong(0));
            }
        }
        return null;
    }
}
